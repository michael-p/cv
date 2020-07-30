mod bicubic;
mod export;

pub use export::*;

use argmin::core::{ArgminKV, ArgminOp, Error, Executor, IterState, Observe, ObserverMode};
use bitarray::BitArray;
use cv_core::nalgebra::{Unit, Vector3, Vector6};
use cv_core::{
    sample_consensus::{Consensus, Estimator},
    Bearing, CameraModel, CameraToCamera, FeatureMatch, FeatureWorldMatch, Pose, Projective,
    TriangulatorObservances, TriangulatorRelative, WorldPoint, WorldToCamera,
};
use cv_optimize::{
    many_view_nelder_mead, single_view_nelder_mead, two_view_nelder_mead, ManyViewConstraint,
    SingleViewConstraint, TwoViewConstraint,
};
use cv_pinhole::{CameraIntrinsicsK1Distortion, EssentialMatrix, NormalizedKeyPoint};
use hnsw::{Searcher, HNSW};
use image::DynamicImage;
use itertools::{izip, Itertools};
use log::*;
use maplit::hashmap;
use ndarray::{array, Array2};
use rand::{seq::SliceRandom, Rng};
use slab::Slab;
use space::Neighbor;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

struct OptimizationObserver;

impl<T: ArgminOp> Observe<T> for OptimizationObserver
where
    T::Param: std::fmt::Debug,
{
    fn observe_iter(&mut self, state: &IterState<T>, _kv: &ArgminKV) -> Result<(), Error> {
        debug!(
            "on iteration {} out of {} with total evaluations {} and current cost {}, params {:?}",
            state.iter, state.max_iters, state.cost_func_count, state.cost, state.param
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Feature {
    pub keypoint: NormalizedKeyPoint,
    pub descriptor: BitArray<64>,
    pub color: [u8; 3],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pair(usize, usize);

impl Pair {
    /// Creates a new pair, cannonicalizing the order of the pair.
    pub fn new(a: usize, b: usize) -> Self {
        Self(std::cmp::min(a, b), std::cmp::max(a, b))
    }
}

#[derive(Clone, Debug)]
pub struct Frame {
    /// A VSlam::feeds index
    pub feed: usize,
    /// The keypoints and corresponding descriptors observed on this frame
    pub features: Vec<Feature>,
}

impl Frame {
    pub fn descriptors(&self) -> impl Iterator<Item = BitArray<64>> + Clone + '_ {
        self.features.iter().map(|f| f.descriptor)
    }

    pub fn keypoint(&self, ix: usize) -> NormalizedKeyPoint {
        self.features[ix].keypoint
    }

    pub fn descriptor(&self, ix: usize) -> &BitArray<64> {
        &self.features[ix].descriptor
    }

    pub fn color(&self, ix: usize) -> [u8; 3] {
        self.features[ix].color
    }
}

/// A 3d point in space that has been observed on two or more frames
#[derive(Debug, Clone)]
pub struct Landmark {
    /// Contains a map from VSlam::views indices to Frame::features indices.
    pub observations: HashMap<usize, usize>,
}

/// A frame which has been incorporated into a reconstruction.
#[derive(Debug, Clone)]
pub struct View {
    /// The VSlam::frame index corresponding to this view
    pub frame: usize,
    /// Pose in the reconstruction of the view
    pub pose: WorldToCamera,
    /// A vector containing the Reconstruction::landmarks indices for each feature in the frame
    pub landmarks: Vec<usize>,
}

/// Frames from a video source
#[derive(Clone, Debug)]
pub struct Feed {
    /// The camera intrinsics for this feed
    intrinsics: CameraIntrinsicsK1Distortion,
    /// VSlam::frames indices corresponding to each frame of the feed
    frames: Vec<usize>,
    /// The VSlam::reconstructions index currently being tracked
    /// If tracking fails, the reconstruction will be set to None.
    reconstruction: Option<usize>,
}

/// A series of views and points which exist in the same world space
#[derive(Clone, Default)]
pub struct Reconstruction {
    /// The VSlam::views IDs contained in this reconstruction
    pub views: Slab<View>,
    /// The landmarks contained in this reconstruction
    pub landmarks: Slab<Landmark>,
    /// The HNSW to look up all landmarks in the reconstruction
    pub descriptor_observations: HNSW<BitArray<64>>,
    /// Vector for each HNSW entry to (Reconstruction::view, Frame::features) indices
    pub observations: Vec<(usize, usize)>,
}

/// Contains the results of a bundle adjust
pub struct BundleAdjust {
    /// The reconstruction the bundle adjust is happening on.
    reconstruction: usize,
    /// Maps VSlam::views IDs to poses
    poses: Vec<(usize, WorldToCamera)>,
}

/// The mapping data for VSlam.
#[derive(Clone, Default)]
pub struct VSlamData {
    /// Contains the camera intrinsics for each feed
    feeds: Slab<Feed>,
    /// Contains each one of the ongoing reconstructions
    reconstructions: Slab<Reconstruction>,
    /// Contains all the frames
    frames: Slab<Frame>,
}

impl VSlamData {
    pub fn feed(&self, feed: usize) -> &Feed {
        &self.feeds[feed]
    }

    pub fn frame(&self, frame: usize) -> &Frame {
        &self.frames[frame]
    }

    pub fn keypoint(&self, frame: usize, feature: usize) -> NormalizedKeyPoint {
        self.frames[frame].keypoint(feature)
    }

    pub fn descriptor(&self, frame: usize, feature: usize) -> &BitArray<64> {
        self.frames[frame].descriptor(feature)
    }

    pub fn color(&self, frame: usize, feature: usize) -> [u8; 3] {
        self.frame(frame).color(feature)
    }

    pub fn reconstruction(&self, reconstruction: usize) -> &Reconstruction {
        &self.reconstructions[reconstruction]
    }

    pub fn view(&self, reconstruction: usize, view: usize) -> &View {
        &self.reconstructions[reconstruction].views[view]
    }

    fn view_mut(&mut self, reconstruction: usize, view: usize) -> &mut View {
        &mut self.reconstructions[reconstruction].views[view]
    }

    pub fn view_frame(&self, reconstruction: usize, view: usize) -> usize {
        self.view(reconstruction, view).frame
    }

    pub fn pose(&self, reconstruction: usize, view: usize) -> WorldToCamera {
        self.view(reconstruction, view).pose
    }

    pub fn observation_landmark(
        &self,
        reconstruction: usize,
        view: usize,
        feature: usize,
    ) -> usize {
        self.reconstructions[reconstruction].views[view].landmarks[feature]
    }

    pub fn observation_color(&self, reconstruction: usize, view: usize, feature: usize) -> [u8; 3] {
        self.color(self.view_frame(reconstruction, view), feature)
    }

    pub fn observation_keypoint(
        &self,
        reconstruction: usize,
        view: usize,
        feature: usize,
    ) -> NormalizedKeyPoint {
        self.keypoint(self.view_frame(reconstruction, view), feature)
    }

    pub fn is_observation_good(
        &self,
        reconstruction: usize,
        view: usize,
        feature: usize,
        point: WorldPoint,
        threshold: f64,
    ) -> bool {
        let bearing = self
            .observation_keypoint(reconstruction, view, feature)
            .bearing();
        let view_point = self.reconstructions[reconstruction].views[view]
            .pose
            .transform(point);
        let residual = 1.0 - bearing.dot(&view_point.bearing());
        // If the observation is finite and has a low enough residual, it is good.
        residual.is_finite() && residual < threshold
    }

    pub fn landmark(&self, reconstruction: usize, landmark: usize) -> &Landmark {
        &self.reconstructions[reconstruction].landmarks[landmark]
    }

    /// Retrieves the (view, feature) iterator from a landmark.
    pub fn landmark_observations(
        &self,
        reconstruction: usize,
        landmark: usize,
    ) -> impl Iterator<Item = (usize, usize)> + Clone + '_ {
        self.landmark(reconstruction, landmark)
            .observations
            .iter()
            .map(|(&view, &feature)| (view, feature))
    }

    /// This checks if a landmark is sufficiently robust by determining if any pair of observations
    /// of this landmark have a cosine distance (in world space) greater than or equal to `minimum_cosine_distance`.
    pub fn is_landmark_robust(
        &self,
        reconstruction: usize,
        landmark: usize,
        minimum_cosine_distance: f64,
    ) -> bool {
        self.landmark_observations(reconstruction, landmark)
            .map(|(view, feature)| {
                let pose = self.pose(reconstruction, view).inverse();
                pose.isometry()
                    * self
                        .observation_keypoint(reconstruction, view, feature)
                        .bearing()
            })
            .tuple_combinations()
            .any(|(bearing_a, bearing_b)| 1.0 - bearing_a.dot(&bearing_b) > minimum_cosine_distance)
    }

    /// Add a [`Reconstruction`] from two initial frames and good matches between their features.
    pub fn add_reconstruction(
        &mut self,
        frame_a: usize,
        frame_b: usize,
        pose: CameraToCamera,
        matches: Vec<FeatureMatch<usize>>,
    ) -> usize {
        // Create a new empty reconstruction.
        let reconstruction = self.reconstructions.insert(Reconstruction::default());
        // Add frame A to new reconstruction using an empty set of landmarks so all features are added as new landmarks.
        let view_a = self.add_view(reconstruction, frame_a, Pose::identity(), |_| None);
        // For all feature matches, create a map from the feature ix .
        let landmarks: HashMap<usize, usize> = matches
            .into_iter()
            .map(|FeatureMatch(feature_a, feature_b)| {
                (
                    feature_b,
                    self.observation_landmark(reconstruction, view_a, feature_a),
                )
            })
            .collect();
        // Add frame B to new reconstruction using the extracted landmark, bix pairs.
        self.add_view(
            reconstruction,
            frame_b,
            WorldToCamera::from(pose.isometry()),
            |feature| landmarks.get(&feature).copied(),
        );
        reconstruction
    }

    /// Adds a new View with no landmarks in it.
    ///
    /// `existing_landmark` is passed a Frame::features index and returns the associated landmark if it exists.
    pub fn add_view(
        &mut self,
        reconstruction: usize,
        frame: usize,
        pose: WorldToCamera,
        existing_landmark: impl Fn(usize) -> Option<usize>,
    ) -> usize {
        let view = self.reconstructions[reconstruction].views.insert(View {
            frame,
            pose,
            landmarks: vec![],
        });

        // Init a searcher only once to avoid allocation durring k-NN searches.
        let mut searcher = Searcher::default();

        // Add all of the view's features to the reconstruction.
        for feature in 0..self.frame(frame).features.len() {
            // Add the feature to the HNSW.
            self.reconstructions[reconstruction]
                .descriptor_observations
                .insert(*self.frames[frame].descriptor(feature), &mut searcher);
            // Add the HNSW index to the HNSW index to the feature landmark map.
            self.reconstructions[reconstruction]
                .observations
                .push((view, feature));
            // Check if the feature is part of an existing landmark.
            let landmark = if let Some(landmark) = existing_landmark(feature) {
                // Add this observation to the observations of this landmark.
                self.reconstructions[reconstruction].landmarks[landmark]
                    .observations
                    .insert(view, feature);
                landmark
            } else {
                // Create the landmark.
                self.add_landmark(reconstruction, view, feature)
            };
            // Add the Reconstruction::landmark index to the feature landmarks vector for this view.
            self.view_mut(reconstruction, view).landmarks.push(landmark);
        }
        view
    }

    /// Creates a new landmark. You must give the landmark at least one observation, as landmarks
    /// without at least one observation are not permitted.
    fn add_landmark(&mut self, reconstruction: usize, view: usize, feature: usize) -> usize {
        self.reconstructions[reconstruction]
            .landmarks
            .insert(Landmark {
                observations: hashmap! {
                    view => feature,
                },
            })
    }

    /// Find the best matching landmark, filtering appropriately.
    ///
    /// Returns a Reconstruction::landmark index.
    fn locate_landmark(
        &self,
        reconstruction: usize,
        frame: usize,
        feature: usize,
        distance_threshold: usize,
        searcher: &mut Searcher,
    ) -> Option<usize> {
        // Find the nearest neighbors.
        let descriptor = self.descriptor(frame, feature);
        let mut neighbors = [Neighbor::invalid(); 1];
        let best_observation = self.reconstructions[reconstruction]
            .descriptor_observations
            .nearest(descriptor, 24, searcher, &mut neighbors)
            .first()
            .cloned()?;
        let best_descriptor = self.reconstructions[reconstruction]
            .descriptor_observations
            .feature(best_observation.index as u32);
        let best_distance = best_descriptor.distance(descriptor);

        // Find the index of the best feature match from the frame to the best landmark descriptor.
        let symmetric_feature = self
            .frame(frame)
            .descriptors()
            .enumerate()
            .min_by_key(|(_, other_descriptor)| best_descriptor.distance(other_descriptor))?
            .0;

        // Ensure the distance is within the threshold and the match is symmetric.
        if best_distance < distance_threshold && symmetric_feature == feature {
            let (view, feature) =
                self.reconstructions[reconstruction].observations[best_observation.index];
            Some(self.reconstructions[reconstruction].views[view].landmarks[feature])
        } else {
            None
        }
    }

    fn apply_bundle_adjust(&mut self, bundle_adjust: BundleAdjust) {
        let BundleAdjust {
            reconstruction,
            poses,
        } = bundle_adjust;
        for (view, pose) in poses {
            self.reconstructions[reconstruction].views[view].pose = pose;
        }
    }

    /// Splits the observation into its own landmark.
    ///
    /// Returns the landmark ID (new or old, as necessary).
    fn split_observation(&mut self, reconstruction: usize, view: usize, feature: usize) -> usize {
        // Check if this is the only observation in the landmark.
        let old_landmark = self.reconstructions[reconstruction].views[view].landmarks[feature];
        if self.reconstructions[reconstruction].landmarks[old_landmark]
            .observations
            .len()
            >= 2
        {
            // Since this wasnt the only observation in the landmark, we can split it.
            // Remove the observation from the old_landmark.
            assert_eq!(
                self.reconstructions[reconstruction].landmarks[old_landmark]
                    .observations
                    .remove(&view),
                Some(feature)
            );
            // Create the new landmark.
            let new_landmark = self.reconstructions[reconstruction]
                .landmarks
                .insert(Landmark {
                    observations: hashmap! {
                        view => feature,
                    },
                });
            // Assign the landmark ID to the observation.
            self.reconstructions[reconstruction].views[view].landmarks[feature] = new_landmark;
            new_landmark
        } else {
            old_landmark
        }
    }

    /// Merges two landmarks unconditionally. Returns the new landmark ID.
    fn merge_landmarks(
        &mut self,
        reconstruction: usize,
        landmark_a: usize,
        landmark_b: usize,
    ) -> usize {
        let old_landmark = self.reconstructions[reconstruction]
            .landmarks
            .remove(landmark_b);
        for (view, feature) in old_landmark.observations {
            // We must start by updating the landmark in the view for this feature.
            self.reconstructions[reconstruction].views[view].landmarks[feature] = landmark_a;
            // Add the observation to landmark A.
            assert!(self.reconstructions[reconstruction].landmarks[landmark_a]
                .observations
                .insert(view, feature)
                .is_none());
        }
        landmark_a
    }
}

/// The settings for the VSlam process.
pub struct VSlamSettings {
    /// The threshold used for akaze
    pub akaze_threshold: f64,
    /// The threshold distance below which a match is allowed
    pub match_threshold: usize,
    /// The number of points to use in optimization of matches
    pub optimization_points: usize,
    /// The minimum cosine distance required of a landmark for it to be considered robust enough for optimization
    pub incidence_minimum_cosine_distance: f64,
    /// The cutoff for the loss function
    pub loss_cutoff: f64,
    /// The maximum cosine distance permitted in a valid match
    pub cosine_distance_threshold: f64,
    /// The threshold of all observations in a landmark relative to another landmark to merge the two.
    pub merge_cosine_distance_threshold: f64,
    /// The maximum iterations to optimize one view.
    pub single_view_patience: usize,
    /// The threshold of mean cosine distance standard deviation that terminates single-view optimization.
    pub single_view_std_dev_threshold: f64,
    /// The cosine distance threshold during initialization.
    pub two_view_cosine_distance_threshold: f64,
    /// The maximum iterations to optimize two views.
    pub two_view_patience: usize,
    /// The threshold of mean cosine distance standard deviation that terminates two-view optimization.
    pub two_view_std_dev_threshold: f64,
    /// The maximum iterations to run two-view optimization and filtering
    pub two_view_filter_loop_iterations: usize,
    /// The maximum number of landmarks to use for pose estimation during tracking.
    pub track_landmarks: usize,
    /// The maximum iterations to optimize many views.
    pub many_view_patience: usize,
    /// The threshold of mean cosine distance standard deviation that terminates many-view optimization.
    pub many_view_std_dev_threshold: f64,
}

impl Default for VSlamSettings {
    fn default() -> Self {
        Self {
            akaze_threshold: 0.001,
            match_threshold: 64,
            optimization_points: 8192,
            incidence_minimum_cosine_distance: 0.0001,
            loss_cutoff: 0.05,
            cosine_distance_threshold: 0.00001,
            merge_cosine_distance_threshold: 0.000005,
            single_view_patience: 8000,
            single_view_std_dev_threshold: 0.0000000001,
            two_view_cosine_distance_threshold: 0.0001,
            two_view_patience: 2000,
            two_view_std_dev_threshold: 0.0000000001,
            two_view_filter_loop_iterations: 3,
            track_landmarks: 4096,
            many_view_patience: 2000,
            many_view_std_dev_threshold: 0.00000001,
        }
    }
}

pub struct VSlam<C, EE, PE, T, R> {
    /// Mapping data
    pub data: VSlamData,
    /// Settings variables
    pub settings: VSlamSettings,
    /// The consensus algorithm
    pub consensus: RefCell<C>,
    /// The essential matrix estimator
    pub essential_estimator: EE,
    /// The PnP estimator
    pub pose_estimator: PE,
    /// The triangulation algorithm
    pub triangulator: T,
    /// The random number generator
    pub rng: RefCell<R>,
}

impl<C, EE, PE, T, R> VSlam<C, EE, PE, T, R>
where
    C: Consensus<EE, FeatureMatch<NormalizedKeyPoint>>
        + Consensus<PE, FeatureWorldMatch<NormalizedKeyPoint>>,
    EE: Estimator<FeatureMatch<NormalizedKeyPoint>, Model = EssentialMatrix>,
    PE: Estimator<FeatureWorldMatch<NormalizedKeyPoint>, Model = WorldToCamera>,
    T: TriangulatorObservances + Clone,
    R: Rng,
{
    /// Creates an empty vSLAM reconstruction.
    pub fn new(
        data: VSlamData,
        settings: VSlamSettings,
        consensus: C,
        essential_estimator: EE,
        pose_estimator: PE,
        triangulator: T,
        rng: R,
    ) -> Self {
        Self {
            data,
            settings,
            consensus: RefCell::new(consensus),
            essential_estimator,
            pose_estimator,
            triangulator,
            rng: RefCell::new(rng),
        }
    }

    /// Adds a new feed with the given intrinsics.
    pub fn add_feed(&mut self, intrinsics: CameraIntrinsicsK1Distortion) -> usize {
        self.data.feeds.insert(Feed {
            intrinsics,
            frames: vec![],
            reconstruction: None,
        })
    }

    /// Add frame.
    ///
    /// This may perform camera tracking and will always extract features.
    ///
    /// Returns a VSlam::reconstructions index if the frame was incorporated in a reconstruction.
    pub fn add_frame(&mut self, feed: usize, image: &DynamicImage) -> Option<usize> {
        // Extract the features for the frame and add the frame object.
        let next_id = self.data.frames.insert(Frame {
            feed,
            features: self.kps_descriptors(&self.data.feeds[feed].intrinsics, image),
        });
        // Add the frame to the feed.
        self.data.feeds[feed].frames.push(next_id);
        // Get the number of frames this feed has.
        let num_frames = self.data.feeds[feed].frames.len();

        if let Some(reconstruction) = self.data.feeds[feed].reconstruction {
            // If the feed has an active reconstruction, try to track the frame.
            if self.try_track(reconstruction, next_id).is_none() {
                // If tracking fails, set the active reconstruction to None.
                self.data.feeds[feed].reconstruction = None;
            }
        } else if num_frames >= 2 {
            // If there is no active reconstruction, but we have at least two frames, try to initialize the reconstruction
            // using the last two frames.
            let frame_a = self.data.feeds[feed].frames[num_frames - 2];
            let frame_b = self.data.feeds[feed].frames[num_frames - 1];
            self.data.feeds[feed].reconstruction = self.try_init(frame_a, frame_b);
        }
        self.data.feeds[feed].reconstruction
    }

    /// Attempts to match a frame pair, creating a new reconstruction from a two view pair.
    ///
    /// Returns the VSlam::reconstructions ID if successful.
    fn try_init(&mut self, frame_a: usize, frame_b: usize) -> Option<usize> {
        // Add the outcome.
        let (pose, matches) = self.init_reconstruction(frame_a, frame_b)?;
        Some(
            self.data
                .add_reconstruction(frame_a, frame_b, pose, matches),
        )
    }

    /// Attempts to track the camera.
    ///
    /// Returns Reconstruction::views index if successful.
    fn try_track(&mut self, reconstruction: usize, frame: usize) -> Option<usize> {
        // Generate the outcome.
        let (pose, landmarks) = self.locate_frame(reconstruction, frame)?;

        // For all feature matches, create a map from the feature ix .
        let landmarks: HashMap<usize, usize> = landmarks
            .into_iter()
            .map(|FeatureMatch(landmark, feature)| (feature, landmark))
            .collect();

        // Add the outcome.
        Some(self.data.add_view(reconstruction, frame, pose, |feature| {
            landmarks.get(&feature).copied()
        }))
    }

    /// Triangulates the point of each match, filtering out matches which fail triangulation or chirality test.
    fn camera_to_camera_match_points<'a>(
        &'a self,
        a: &'a Frame,
        b: &'a Frame,
        pose: CameraToCamera,
        matches: impl Iterator<Item = FeatureMatch<usize>> + 'a,
    ) -> impl Iterator<Item = FeatureMatch<usize>> + 'a {
        matches.filter_map(move |m| {
            let FeatureMatch(a, b) = FeatureMatch(a.keypoint(m.0), b.keypoint(m.1));
            let point_a = self.triangulator.triangulate_relative(pose, a, b)?;
            let point_b = pose.transform(point_a);
            let camera_b_bearing_a = pose.isometry() * a.bearing();
            let camera_b_bearing_b = b.bearing();
            let residual = 1.0 - point_a.bearing().dot(&a.bearing()) + 1.0
                - point_b.bearing().dot(&b.bearing());
            let incidence_cosine_distance = 1.0 - camera_b_bearing_a.dot(&camera_b_bearing_b);
            if residual.is_finite()
                && (residual < self.settings.two_view_cosine_distance_threshold
                    && point_a.z.is_sign_positive()
                    && point_b.z.is_sign_positive()
                    && incidence_cosine_distance > self.settings.incidence_minimum_cosine_distance)
            {
                Some(m)
            } else {
                None
            }
        })
    }

    /// This creates a covisibility between frames `a` and `b` using the essential matrix estimator.
    ///
    /// This method resolves to an undefined scale, and thus is only appropriate for initialization.
    fn init_reconstruction(
        &self,
        frame_a: usize,
        frame_b: usize,
    ) -> Option<(CameraToCamera, Vec<FeatureMatch<usize>>)> {
        let a = self.data.frame(frame_a);
        let b = self.data.frame(frame_b);
        // A helper to convert an index match to a keypoint match given frame a and b.
        let match_ix_kps = |FeatureMatch(feature_a, feature_b)| {
            FeatureMatch(a.keypoint(feature_a), b.keypoint(feature_b))
        };

        info!(
            "performing brute-force matching between {} and {} features",
            a.features.len(),
            b.features.len(),
        );
        // Retrieve the matches which agree with each other from each frame and filter out ones that aren't within the match threshold.
        let matches: Vec<FeatureMatch<usize>> = symmetric_matching(a, b)
            .filter(|&(_, distance)| distance < self.settings.match_threshold)
            .map(|(m, _)| m)
            .collect();

        let original_matches = matches.clone();

        info!("estimate essential on {} matches", matches.len());

        // Estimate the essential matrix and retrieve the inliers
        let (essential, inliers) = self.consensus.borrow_mut().model_inliers(
            &self.essential_estimator,
            matches
                .iter()
                .copied()
                .map(match_ix_kps)
                .collect::<Vec<_>>()
                .iter()
                .copied(),
        )?;
        // Reconstitute only the inlier matches into a matches vector.
        let matches: Vec<FeatureMatch<usize>> = inliers.into_iter().map(|ix| matches[ix]).collect();

        info!("perform chirality test on {}", matches.len());

        // Perform a chirality test to retain only the points in front of both cameras.
        let mut pose = essential
            .pose_solver()
            .solve_unscaled(matches.iter().copied().map(match_ix_kps))?;

        // Initialize the camera points.
        let mut matches: Vec<FeatureMatch<usize>> = self
            .camera_to_camera_match_points(a, b, pose, original_matches.iter().copied())
            .collect();

        for _ in 0..self.settings.two_view_filter_loop_iterations {
            let opti_matches: Vec<FeatureMatch<NormalizedKeyPoint>> = matches
                .choose_multiple(
                    &mut *self.rng.borrow_mut(),
                    self.settings.optimization_points,
                )
                .copied()
                .map(match_ix_kps)
                .collect::<Vec<_>>();

            info!(
                "performing Nelder-Mead optimization on pose using {} matches out of {}",
                opti_matches.len(),
                matches.len()
            );

            let solver =
                two_view_nelder_mead(pose).sd_tolerance(self.settings.two_view_std_dev_threshold);
            let constraint =
                TwoViewConstraint::new(opti_matches.iter().copied(), self.triangulator.clone())
                    .loss_cutoff(self.settings.loss_cutoff);

            // The initial parameter is empty becasue nelder mead is passed its own initial parameter directly.
            let opti_state = Executor::new(constraint, solver, array![])
                .add_observer(OptimizationObserver, ObserverMode::Always)
                .max_iters(self.settings.two_view_patience as u64)
                .run()
                .expect("two-view optimization failed")
                .state;

            info!(
                "extracted pose with mean capped cosine distance of {}",
                opti_state.best_cost
            );

            pose = Pose::from_se3(Vector6::from_row_slice(
                opti_state
                    .best_param
                    .as_slice()
                    .expect("param was not contiguous array"),
            ));

            // Filter outlier matches based on cosine distance.
            matches = self
                .camera_to_camera_match_points(a, b, pose, original_matches.iter().copied())
                .collect();

            info!("filtering left us with {} matches", matches.len());
        }

        info!(
            "matches remaining after all filtering stages: {}",
            matches.len()
        );

        // Add the new covisibility.
        Some((pose, matches))
    }

    /// Attempts to track the frame in the reconstruction.
    ///
    /// Returns the pose and a vector of indices in the format (Reconstruction::landmarks, Frame::features).
    fn locate_frame(
        &self,
        reconstruction: usize,
        frame: usize,
    ) -> Option<(WorldToCamera, Vec<FeatureMatch<usize>>)> {
        info!("find existing landmarks to track camera");
        // Start by trying to match the frame's features to the landmarks in the reconstruction.
        // Get back a bunch of (Reconstruction::landmarks, Frame::features) correspondences.
        let mut searcher = Searcher::default();
        let matches: Vec<FeatureMatch<usize>> = (0..self.data.frame(frame).features.len())
            .filter_map(|feature| {
                self.data
                    .locate_landmark(
                        reconstruction,
                        frame,
                        feature,
                        self.settings.match_threshold,
                        &mut searcher,
                    )
                    .map(|landmark| FeatureMatch(landmark, feature))
            })
            .collect();

        info!("removing any landmarks which matched more than one feature");
        // Create counts of how often each landmark appears.
        let mut landmark_counts: HashMap<usize, usize> = HashMap::new();
        for &FeatureMatch(landmark, _) in &matches {
            *landmark_counts.entry(landmark).or_default() += 1;
        }
        let matches: Vec<FeatureMatch<usize>> = matches
            .into_iter()
            .filter(|&FeatureMatch(landmark, _)| landmark_counts[&landmark] == 1)
            .collect();

        info!("found {} suitable landmark matches", matches.len());

        let create_3d_matches = |required_observations| {
            matches
                .choose_multiple(&mut *self.rng.borrow_mut(), matches.len())
                .filter(|&&FeatureMatch(landmark, _)| {
                    self.data
                        .landmark(reconstruction, landmark)
                        .observations
                        .len()
                        >= required_observations
                        && self.data.is_landmark_robust(
                            reconstruction,
                            landmark,
                            self.settings.incidence_minimum_cosine_distance,
                        )
                })
                .filter_map(|&FeatureMatch(landmark, feature)| {
                    Some(FeatureWorldMatch(
                        self.data.keypoint(frame, feature),
                        self.triangulate_landmark(reconstruction, landmark)?,
                    ))
                })
                .take(self.settings.track_landmarks)
                .collect()
        };

        // Extract the FeatureWorldMatch for each of the features.
        let matches_3d: Vec<FeatureWorldMatch<NormalizedKeyPoint>> = create_3d_matches(3);

        let matches_3d = if matches_3d.len() < 32 {
            info!("unable to find enough 3d matches with 3 observations, trying 2");
            create_3d_matches(2)
        } else {
            matches_3d
        };

        if matches_3d.len() < 32 {
            info!("unable to find enough 3d matches to track frame");
            return None;
        }

        info!(
            "estimate the pose of the camera using {} triangulatable landmarks",
            matches_3d.len()
        );

        // Estimate the pose and retrieve the inliers.
        let pose = self
            .consensus
            .borrow_mut()
            .model(&self.pose_estimator, matches_3d.iter().copied())?;

        // Create solver and constraint for single-view optimizer.
        let solver =
            single_view_nelder_mead(pose).sd_tolerance(self.settings.single_view_std_dev_threshold);
        let constraint =
            SingleViewConstraint::new(matches_3d).loss_cutoff(self.settings.loss_cutoff);

        // The initial parameter is empty becasue nelder mead is passed its own initial parameter directly.
        let opti_state = Executor::new(constraint, solver, array![])
            .add_observer(OptimizationObserver, ObserverMode::Always)
            .max_iters(self.settings.single_view_patience as u64)
            .run()
            .expect("single-view optimization failed")
            .state;

        info!(
            "extracted single-view pose with mean capped cosine distance of {}",
            opti_state.best_cost
        );

        let pose = Pose::from_se3(Vector6::from_row_slice(
            opti_state
                .best_param
                .as_slice()
                .expect("param was not contiguous array"),
        ));

        // Filter outlier matches and return all others for inclusion.
        let matches = matches
            .into_iter()
            .filter(|&FeatureMatch(landmark, feature)| {
                let keypoint = self.data.keypoint(frame, feature);
                self.triangulate_landmark_with_appended_observation_and_verify_existing_observations(
                    reconstruction,
                    landmark,
                    pose,
                    keypoint,
                )
                .map(|world_point| {
                    // Also verify the new observation.
                    let camera_point = pose.transform(world_point);
                    let bearing = keypoint.bearing();
                    let residual = 1.0 - bearing.dot(&camera_point.bearing());
                    residual.is_finite() && residual < self.settings.cosine_distance_threshold
                })
                .unwrap_or(false)
            })
            .collect();

        Some((pose, matches))
    }

    fn kps_descriptors(
        &self,
        intrinsics: &CameraIntrinsicsK1Distortion,
        image: &DynamicImage,
    ) -> Vec<Feature> {
        let (keypoints, descriptors) =
            akaze::Akaze::new(self.settings.akaze_threshold).extract(image);
        let rbg_image = image.to_rgb();

        // Use bicubic interpolation to extract colors from the image.
        let colors: Vec<[u8; 3]> = keypoints
            .iter()
            .map(|kp| {
                use image::Rgb;
                let (x, y) = kp.point;
                let Rgb(color) = bicubic::interpolate_bicubic(&rbg_image, x, y, Rgb([0, 0, 0]));
                color
            })
            .collect();

        // Calibrate keypoint and combine into features.
        izip!(
            keypoints.into_iter().map(|kp| intrinsics.calibrate(kp)),
            descriptors,
            colors
        )
        .map(|(keypoint, descriptor, color)| Feature {
            keypoint,
            descriptor,
            color,
        })
        .collect()
    }

    pub fn export_reconstruction(
        &self,
        reconstruction: usize,
        minimum_observances: usize,
        minimum_cosine_distance: f64,
        path: impl AsRef<Path>,
    ) {
        // Output point cloud.
        let points_and_colors = self
            .data
            .reconstruction(reconstruction)
            .landmarks
            .iter()
            .filter_map(|(landmark, lm_object)| {
                if lm_object.observations.len() >= minimum_observances
                    && self.data.is_landmark_robust(
                        reconstruction,
                        landmark,
                        minimum_cosine_distance,
                    )
                {
                    self.triangulate_landmark(reconstruction, landmark)
                        .and_then(Projective::point)
                        .map(|p| {
                            let (&view, &feature) = lm_object.observations.iter().next().unwrap();
                            (
                                p,
                                self.data.observation_color(reconstruction, view, feature),
                            )
                        })
                } else {
                    None
                }
            })
            .collect();
        crate::export::export(std::fs::File::create(path).unwrap(), points_and_colors);
    }

    /// Optimizes the entire reconstruction.
    ///
    /// Use `num_landmarks` to control the number of landmarks used in optimization.
    pub fn bundle_adjust_highest_observances(
        &mut self,
        reconstruction: usize,
        num_landmarks: usize,
    ) {
        self.apply_bundle_adjust(
            self.compute_bundle_adjust_highest_observances(reconstruction, num_landmarks),
        );
    }

    /// Optimizes the entire reconstruction.
    ///
    /// Use `num_landmarks` to control the number of landmarks used in optimization.
    ///
    /// Returns a series of camera
    fn compute_bundle_adjust_highest_observances(
        &self,
        reconstruction: usize,
        num_landmarks: usize,
    ) -> BundleAdjust {
        // At least one landmark exists or the unwraps below will fail.
        if !self
            .data
            .reconstruction(reconstruction)
            .landmarks
            .is_empty()
        {
            info!(
                "attempting to extract {} landmarks from a total of {}",
                num_landmarks,
                self.data.reconstruction(reconstruction).landmarks.len(),
            );

            // First, we want to find the landmarks with the most observances to optimize the reconstruction.
            // Start by putting all the landmark indices into a BTreeMap with the key as their observances and the value the index.
            let mut landmarks_by_observances: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for (observations, landmark) in self
                .data
                .reconstruction(reconstruction)
                .landmarks
                .iter()
                .map(|(landmark, lm)| (lm.observations.len(), landmark))
                .filter(|&(observations, landmark)| {
                    observations >= 3
                        && self.data.is_landmark_robust(
                            reconstruction,
                            landmark,
                            self.settings.incidence_minimum_cosine_distance,
                        )
                })
            {
                // Only add landmarks with at least 3 observations.
                landmarks_by_observances
                    .entry(observations)
                    .or_default()
                    .push(landmark);
            }

            info!(
                "found landmarks with (observations, num) of {:?}",
                landmarks_by_observances
                    .iter()
                    .map(|(ob, v)| (ob, v.len()))
                    .collect::<Vec<_>>()
            );

            // Now the BTreeMap is sorted from smallest number of observances to largest, so take the last indices.
            let mut opti_landmarks: Vec<usize> = vec![];
            for bucket in landmarks_by_observances.values().rev() {
                if opti_landmarks.len() + bucket.len() >= num_landmarks {
                    // Add what we need to randomly (to prevent patterns in data that throw off optimization).
                    opti_landmarks.extend(
                        bucket
                            .choose_multiple(
                                &mut *self.rng.borrow_mut(),
                                num_landmarks - opti_landmarks.len(),
                            )
                            .copied(),
                    );
                    break;
                } else {
                    // Add everything from the bucket.
                    opti_landmarks.extend(bucket.iter().copied());
                }
            }

            if opti_landmarks.len() < 32 {
                info!(
                    "insufficient landmarks ({}), need 32; skipping bundle adjust",
                    opti_landmarks.len()
                );
                return BundleAdjust {
                    reconstruction,
                    poses: vec![],
                };
            }

            // Find all the view IDs corresponding to the landmarks.
            let views: Vec<usize> = opti_landmarks
                .iter()
                .copied()
                .flat_map(|landmark| {
                    self.data
                        .landmark(reconstruction, landmark)
                        .observations
                        .iter()
                        .map(|(&view, _)| view)
                })
                .collect::<BTreeSet<usize>>()
                .into_iter()
                .collect();

            // Form a vector over each landmark that contains a vector of the observances present in each view ID in order above.
            let observances: Vec<Vec<Option<Unit<Vector3<f64>>>>> = opti_landmarks
                .iter()
                .copied()
                .map(|landmark| {
                    views
                        .iter()
                        .copied()
                        .map(|view| {
                            self.data
                                .landmark(reconstruction, landmark)
                                .observations
                                .get(&view)
                                .map(|&feature| {
                                    self.data
                                        .observation_keypoint(reconstruction, view, feature)
                                        .bearing()
                                })
                        })
                        .collect()
                })
                .collect();

            // Retrieve the view poses
            let poses: Vec<WorldToCamera> = views
                .iter()
                .copied()
                .map(|view| self.data.pose(reconstruction, view))
                .collect();

            info!(
                "performing Nelder-Mead optimization on {} poses with {} landmarks",
                views.len(),
                opti_landmarks.len(),
            );

            let solver = many_view_nelder_mead(poses)
                .sd_tolerance(self.settings.many_view_std_dev_threshold);
            let constraint = ManyViewConstraint::new(
                observances.iter().map(|v| v.iter().copied()),
                self.triangulator.clone(),
            )
            .loss_cutoff(self.settings.loss_cutoff);

            // The initial parameter is empty becasue nelder mead is passed its own initial parameter directly.
            let opti_state = Executor::new(constraint, solver, Array2::zeros((0, 0)))
                .add_observer(OptimizationObserver, ObserverMode::Always)
                .max_iters(self.settings.many_view_patience as u64)
                .run()
                .expect("many-view optimization failed")
                .state;

            info!(
                "extracted poses with mean capped cosine distance of {}",
                opti_state.best_cost
            );

            let poses: Vec<WorldToCamera> = opti_state
                .best_param
                .outer_iter()
                .map(|arr| {
                    Pose::from_se3(Vector6::from_row_slice(
                        arr.as_slice().expect("param was not contiguous array"),
                    ))
                })
                .collect();

            BundleAdjust {
                reconstruction,
                poses: views.iter().copied().zip(poses).collect(),
            }
        } else {
            warn!(
                "tried to bundle adjust reconstruction with no landmarks, which should not exist"
            );
            BundleAdjust {
                reconstruction,
                poses: vec![],
            }
        }
    }

    fn apply_bundle_adjust(&mut self, bundle_adjust: BundleAdjust) {
        self.data.apply_bundle_adjust(bundle_adjust);
    }

    /// Splits all observations in the landmark into their own separate landmarks.
    fn split_landmark(&mut self, reconstruction: usize, landmark: usize) {
        let observations: Vec<(usize, usize)> = self
            .data
            .landmark_observations(reconstruction, landmark)
            .collect();
        // Don't split the first observation off, as it can stay as this landmark.
        for &(view, feature) in &observations[1..] {
            self.data.split_observation(reconstruction, view, feature);
        }
    }

    pub fn filter_observations(&mut self, reconstruction: usize, threshold: f64) {
        info!("filtering reconstruction observations");
        let landmarks: Vec<usize> = self
            .data
            .reconstruction(reconstruction)
            .landmarks
            .iter()
            .map(|(lmix, _)| lmix)
            .collect();

        // Log the data before filtering.
        let num_triangulatable_landmarks: usize = self
            .data
            .reconstruction(reconstruction)
            .landmarks
            .iter()
            .filter(|&(_, lm)| lm.observations.len() >= 2)
            .count();
        info!(
            "started with {} triangulatable landmarks",
            num_triangulatable_landmarks,
        );

        for landmark in landmarks {
            if let Some(point) = self.triangulate_landmark(reconstruction, landmark) {
                let observations: Vec<(usize, usize)> = self
                    .data
                    .landmark_observations(reconstruction, landmark)
                    .collect();

                for (view, feature) in observations {
                    if !self.data.is_observation_good(
                        reconstruction,
                        view,
                        feature,
                        point,
                        threshold,
                    ) {
                        // If the observation is bad, we must remove it from the landmark and the view.
                        self.data.split_observation(reconstruction, view, feature);
                    }
                }
            } else {
                self.split_landmark(reconstruction, landmark);
            }
        }

        // Log the data after filtering.
        let num_triangulatable_landmarks: usize = self
            .data
            .reconstruction(reconstruction)
            .landmarks
            .iter()
            .filter(|&(_, lm)| lm.observations.len() >= 2)
            .count();
        info!(
            "ended with {} triangulatable landmarks",
            num_triangulatable_landmarks,
        );
    }

    /// Filters landmarks that arent robust via observation incidence angle.
    ///
    /// This filtering stage is optional, and omitting it can be beneficial
    /// when you want to potentially keep landmarks that are very far in the distance.
    /// Triangulating them will still be erroneous, but you can compute the direction these
    /// landmarks are in with relatively good accuracy.
    pub fn filter_non_robust_landmarks(&mut self, reconstruction: usize) {
        info!("filtering non-robust landmarks");
        let landmarks: Vec<usize> = self
            .data
            .reconstruction(reconstruction)
            .landmarks
            .iter()
            .map(|(lmix, _)| lmix)
            .collect();

        // Log the data before filtering.
        let num_triangulatable_landmarks: usize = self
            .data
            .reconstruction(reconstruction)
            .landmarks
            .iter()
            .filter(|&(_, lm)| lm.observations.len() >= 2)
            .count();
        info!(
            "started with {} triangulatable landmarks",
            num_triangulatable_landmarks,
        );

        // Split any landmark that isnt robust.
        for landmark in landmarks {
            if !self.data.is_landmark_robust(
                reconstruction,
                landmark,
                self.settings.incidence_minimum_cosine_distance,
            ) {
                self.split_landmark(reconstruction, landmark);
            }
        }

        // Log the data after filtering.
        let num_triangulatable_landmarks: usize = self
            .data
            .reconstruction(reconstruction)
            .landmarks
            .iter()
            .filter(|&(_, lm)| lm.observations.len() >= 2)
            .count();
        info!(
            "ended with {} triangulatable landmarks",
            num_triangulatable_landmarks,
        );
    }

    /// Attempts to merge two landmarks. If it succeeds, it returns the landmark ID.
    fn try_merge_landmarks(
        &mut self,
        reconstruction: usize,
        landmark_a: usize,
        landmark_b: usize,
    ) -> Option<usize> {
        // If the same view appears in each landmark, then that means two different features from the same view
        // would appear in the resulting landmark, which is invalid.
        let duplicate_view = self
            .data
            .landmark_observations(reconstruction, landmark_a)
            .any(|(view_a, _)| {
                self.data
                    .landmark_observations(reconstruction, landmark_b)
                    .any(|(view_b, _)| view_a == view_b)
            });
        if duplicate_view {
            // We got a duplicate view, so return none.
            return None;
        }
        // Get an iterator over all the observations in both landmarks.
        let all_observations = self
            .data
            .landmark_observations(reconstruction, landmark_a)
            .chain(self.data.landmark_observations(reconstruction, landmark_b));

        // Triangulate the point which would be the combination of all landmarks.
        let point = self.triangulate_observations(reconstruction, all_observations.clone())?;

        // Determine if all observations would be good if merged.
        let all_good = all_observations.clone().all(|(view, feature)| {
            self.data.is_observation_good(
                reconstruction,
                view,
                feature,
                point,
                self.settings.merge_cosine_distance_threshold,
            )
        });
        // Non-lexical lifetimes failed me.
        drop(all_observations);

        if all_good {
            // If they would all be good, merge them.
            Some(
                self.data
                    .merge_landmarks(reconstruction, landmark_a, landmark_b),
            )
        } else {
            // If they would not all be good, dont merge them.
            None
        }
    }

    pub fn merge_nearby_landmarks(&mut self, reconstruction: usize) {
        use rstar::primitives::PointWithData;
        use rstar::RTree;
        type LandmarkPoint = PointWithData<usize, [f64; 3]>;
        info!("merging reconstruction landmarks");
        // Only take landmarks with at least two observations.
        let landmarks: Vec<LandmarkPoint> = self
            .data
            .reconstruction(reconstruction)
            .landmarks
            .iter()
            .filter_map(|(landmark, _)| {
                self.triangulate_landmark(reconstruction, landmark)
                    .and_then(|wp| {
                        wp.point()
                            .map(|p| LandmarkPoint::new(landmark, [p.x, p.y, p.z]))
                    })
            })
            .collect();
        let landmark_index: RTree<LandmarkPoint> = RTree::bulk_load(landmarks.clone());

        let mut num_merged = 0usize;
        for landmark_point_a in landmarks {
            // Check if landmark a still exists.
            if self
                .data
                .reconstruction(reconstruction)
                .landmarks
                .contains(landmark_point_a.data)
            {
                // If the landmark still exists, search its nearest neighbors (up to 4, the first is itself).
                let position: &[f64; 3] = landmark_point_a.position();
                for landmark_point_b in landmark_index.nearest_neighbor_iter(position).take(5) {
                    // Check if it is not matched to itself, if landmark b still exists, and if merging was successful.
                    if landmark_point_a.data != landmark_point_b.data
                        && self
                            .data
                            .reconstruction(reconstruction)
                            .landmarks
                            .contains(landmark_point_b.data)
                        && self
                            .try_merge_landmarks(
                                reconstruction,
                                landmark_point_a.data,
                                landmark_point_b.data,
                            )
                            .is_some()
                    {
                        num_merged += 1;
                    }
                }
            }
        }
        info!("merged {} landmarks", num_merged);
    }

    pub fn triangulate_landmark(
        &self,
        reconstruction: usize,
        landmark: usize,
    ) -> Option<WorldPoint> {
        // TODO: Don't need to check this once https://github.com/rust-cv/cv-geom/issues/1 is fixed.
        if self
            .data
            .landmark(reconstruction, landmark)
            .observations
            .len()
            >= 2
        {
            self.triangulate_observations(
                reconstruction,
                self.data.landmark_observations(reconstruction, landmark),
            )
        } else {
            None
        }
    }

    pub fn triangulate_landmark_with_appended_observation(
        &self,
        reconstruction: usize,
        landmark: usize,
        pose: WorldToCamera,
        keypoint: NormalizedKeyPoint,
    ) -> Option<WorldPoint> {
        self.triangulator.triangulate_observances(
            self.data
                .landmark_observations(reconstruction, landmark)
                .map(|(view, feature)| {
                    (
                        self.data.pose(reconstruction, view),
                        self.data
                            .observation_keypoint(reconstruction, view, feature),
                    )
                })
                .chain(std::iter::once((pose, keypoint))),
        )
    }

    pub fn triangulate_landmark_with_appended_observation_and_verify_existing_observations(
        &self,
        reconstruction: usize,
        landmark: usize,
        pose: WorldToCamera,
        keypoint: NormalizedKeyPoint,
    ) -> Option<WorldPoint> {
        self.triangulate_landmark_with_appended_observation(
            reconstruction,
            landmark,
            pose,
            keypoint,
        )
        .filter(|world_point| {
            self.data
                .landmark_observations(reconstruction, landmark)
                .all(|(view, feature)| {
                    let pose = self.data.pose(reconstruction, view);
                    let camera_point = pose.transform(*world_point);
                    let keypoint = self
                        .data
                        .observation_keypoint(reconstruction, view, feature);
                    let residual = 1.0 - keypoint.bearing().dot(&camera_point.bearing());
                    residual.is_finite() && residual < self.settings.cosine_distance_threshold
                })
        })
    }

    /// Triangulates a landmark with observations added. An observation is a (view, feature) pair.
    pub fn triangulate_observations(
        &self,
        reconstruction: usize,
        observations: impl Iterator<Item = (usize, usize)>,
    ) -> Option<WorldPoint> {
        self.triangulator
            .triangulate_observances(observations.map(|(view, feature)| {
                (
                    self.data.pose(reconstruction, view),
                    self.data
                        .observation_keypoint(reconstruction, view, feature),
                )
            }))
    }

    /// Use this gratuitously to help debug.
    ///
    /// This is useful when the system gets into an inconsistent state due to an internal
    /// bug. This kind of issue can't be tracked down by debugging, since you have to rewind
    /// backwards and look for connections between data to understand where the issue went wrong.
    /// By using this, you can observe errors as they accumulate in the system to better track them down.
    pub fn sanity_check(&self, reconstruction: usize) {
        info!("SANITY CHECK: checking to see if all view landmarks still exist");
        for view in self
            .data
            .reconstruction(reconstruction)
            .views
            .iter()
            .map(|(view, _)| view)
        {
            for (feature, &landmark) in self.data.reconstruction(reconstruction).views[view]
                .landmarks
                .iter()
                .enumerate()
            {
                if !self
                    .data
                    .reconstruction(reconstruction)
                    .landmarks
                    .contains(landmark)
                {
                    error!("SANITY CHECK FAILURE: landmark associated with reconstruction {}, view {}, and feature {} does not exist, it was landmark {}", reconstruction, view, feature, landmark);
                } else {
                    let observation = self.data.reconstruction(reconstruction).landmarks[landmark]
                        .observations
                        .get(&view);
                    if observation != Some(&feature) {
                        error!("SANITY CHECK FAILURE: landmark associated with reconstruction {}, view {}, and feature {} does not contain the feature as an observation, instead found feature {:?}", reconstruction, view, feature, observation);
                    }
                }
            }
        }
        info!("SANITY CHECK ENDED");
    }
}

fn matching(
    a_descriptors: impl Iterator<Item = BitArray<64>>,
    b_descriptors: impl Iterator<Item = BitArray<64>> + Clone,
) -> Vec<(usize, usize)> {
    a_descriptors
        .map(|a| {
            b_descriptors
                .clone()
                .map(|b| a.distance(&b))
                .enumerate()
                .min_by_key(|&(_, d)| d)
                .unwrap()
        })
        .collect::<Vec<_>>()
}

fn symmetric_matching<'a>(
    a: &'a Frame,
    b: &'a Frame,
) -> impl Iterator<Item = (FeatureMatch<usize>, usize)> + 'a {
    // The best match for each feature in frame a to frame b's features.
    let forward_matches = matching(a.descriptors(), b.descriptors());
    // The best match for each feature in frame b to frame a's features.
    let reverse_matches = matching(b.descriptors(), a.descriptors());
    forward_matches
        .into_iter()
        .enumerate()
        .filter_map(move |(aix, (bix, distance))| {
            // Does the feature in b match with this feature too?
            let is_symmetric = reverse_matches[bix].0 == aix;
            if is_symmetric {
                Some((FeatureMatch(aix, bix), distance))
            } else {
                None
            }
        })
}