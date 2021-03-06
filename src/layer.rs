// mlodato, 20190806

use crate::geom::{
    Bounds,
    BoxTestGeometry,
    IndexGenerator,
    RayTestGeometry,
    SystemBounds,
    TestGeometry,
    VecDim,
};
use crate::index::SpatialIndex;
use crate::traits::ObjectID;

use cgmath::prelude::*;
use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use std::fmt::Debug;
use std::ops::DerefMut;

#[cfg(feature="parallel")]
use rayon::prelude::*;

#[cfg(feature="parallel")]
use std::cell::{RefMut, RefCell};

#[cfg(feature="parallel")]
use thread_local::CachedThreadLocal;

/// [`SpatialIndex`]: trait.SpatialIndex.html
/// [`Index64_3D`]: struct.Index64_3D.html

/// A group of collision data
/// 
/// `Index` must be a type implmenting [`SpatialIndex`], such as [`Index64_3D`]
/// 
/// `ID` is the type representing object IDs

#[derive(Default)]
#[cfg_attr(any(test, feature="serde"), derive(Deserialize, Serialize))]
pub struct Layer<Index, ID>
where
    Index: SpatialIndex,
    ID: ObjectID,
    Bounds<Index::Point>: IndexGenerator<Index>
{
    // persistant state:
    min_depth: u32,
    tree: (Vec<(Index, ID)>, bool),

    // temporary data used within a method:
    #[cfg_attr(any(test, feature="serde"), serde(skip))]
    collisions: Vec<(ID, ID)>,

    #[cfg_attr(any(test, feature="serde"), serde(skip))]
    test_results: Vec<ID>,

    #[cfg_attr(any(test, feature="serde"), serde(skip))]
    processed: FxHashSet<ID>,

    #[cfg_attr(any(test, feature="serde"), serde(skip))]
    invalid: Vec<ID>,

    #[cfg(feature="parallel")]
    #[cfg_attr(any(test, feature="serde"), serde(skip))]
    collisions_tls: CachedThreadLocal<RefCell<Vec<(ID, ID)>>>,
}

impl<Index, ID> Layer<Index, ID>
where
    Index: SpatialIndex,
    ID: ObjectID,
    Bounds<Index::Point>: IndexGenerator<Index>
{
    /// Iterate over all indices in the `Layer`
    /// 
    /// This is primarily intended for visualization + debugging
    pub fn iter(&self) -> std::slice::Iter<'_, (Index, ID)> {
        self.tree.0.iter()
    }

    /// Clear all index-ID pairs
    pub fn clear(&mut self) {
        let (tree, sorted) = &mut self.tree;
        tree.clear();
        *sorted = true;
    }

    /// Append multiple objects to the `Layer`
    /// 
    /// Complex geometry may provide multiple bounds for a single object ID; this usage would be common
    /// for static geometry, as it prevents extraneous self-collisions
    pub fn extend<Iter, Point_>(&mut self, system_bounds: Bounds<Point_>, objects: Iter)
    where
        Iter: std::iter::Iterator<Item = (Bounds<Point_>, ID)>,
        Point_: EuclideanSpace<Scalar = f32>,
        Point_::Diff: ElementWise,
        Bounds<Point_>: SystemBounds<Point_, Index::Point>
    {
        let (tree, sorted) = &mut self.tree;

        if let (_, Some(max_objects)) = objects.size_hint() {
            tree.reserve(max_objects);
        }

        for (bounds, id) in objects {
            if !system_bounds.contains(bounds) {
                self.invalid.push(id);
                continue
            }

            tree.extend(system_bounds
                .to_local(bounds)
                .indices(Some(self.min_depth))
                .into_iter()
                .map(|index| (index, id)));

            *sorted = false;
        }
    }

    /// Merge another `Layer` into this `Layer`
    /// 
    /// This may be used, for example, to merge static scene `Layer` into the current
    /// frames' dynamic `Layer` without having to recalculate indices for the static data
    pub fn merge(&mut self, other: &Layer<Index, ID>) {
        let (lhs_tree, sorted) = &mut self.tree;
        let (rhs_tree, _) = &other.tree;

        if other.min_depth < self.min_depth {
            warn!("merging layer of lesser min_depth (lhs: {}, rhs: {})", self.min_depth, other.min_depth);
            self.min_depth = other.min_depth;
        }

        lhs_tree.extend(rhs_tree.iter());
        *sorted = false;
    }

    /// [`par_scan_filtered`]: struct.Layer.html#method.par_scan_filtered
    /// [`par_scan`]: struct.Layer.html#method.par_scan
    /// Sort indices to ready data for detection (parallel)
    /// 
    /// This will be called implicitly when necessary (i.e. by [`par_scan_filtered`], [`par_scan`], etc.)
    #[cfg(feature="parallel")]
    pub fn par_sort(&mut self) {
        let (tree, sorted) = &mut self.tree;
        if !*sorted {
            tree.par_sort_unstable();
            *sorted = true;
        }
    }

    /// [`scan_filtered`]: struct.Layer.html#method.scan_filtered
    /// [`scan`]: struct.Layer.html#method.scan
    /// Sort indices to ready data for detection
    /// 
    /// This will be called implicitly when necessary (i.e. by [`scan_filtered`], [`scan`], etc.)
    pub fn sort(&mut self) {
        let (tree, sorted) = &mut self.tree;
        if !*sorted {
            tree.sort_unstable();
            *sorted = true;
        }
    }

    fn test_impl<TestGeom, Callback>(
        tree: &[(Index, ID)],
        cell: Index,
        test_geom: &TestGeom,
        mut nearest: f32,
        max_depth: Option<u32>,
        callback: &mut Callback) -> f32
    where
        TestGeom: TestGeometry,
        Callback: FnMut(&TestGeom, f32, ID) -> f32
    {
        use std::cmp::Ordering::{Less, Greater};

        if tree.is_empty() || !test_geom.should_test(nearest) {
            return nearest;
        }

        if tree.first().unwrap().0 < cell || !cell.overlaps(tree.last().unwrap().0) {
            panic!("test_impl called with non-overlapping indices");
        }

        let depth = cell.depth();
        if let Some(max_depth) = max_depth {
            if depth >= max_depth {
                return tree.iter()
                    .map(|(_, id)| *id)
                    .fold(nearest, |nearest, id|
                        callback(test_geom, nearest, id).min(nearest));
            }
        }

        if let Some(sub_cells) = cell.subdivide() {
            let mut sub_trees = sub_cells.as_ref().iter()
                .map(|cell| Some(*cell))
                .chain((0..1).map(|_| None))
                .scan(tree, |tree, cell| {
                    if let Some(cell) = cell {
                        let i = tree.binary_search_by(|&(index, _)| {
                            if index < cell { Less } else { Greater }
                        }).err().unwrap();
                        let (head, tail) = tree.split_at(i);
                        *tree = tail;
                        Some(head)
                    } else {
                        Some(tree)
                    }
                });
            nearest = sub_trees.next().unwrap().iter()
                .map(|(_, id)| *id)
                .fold(nearest, |nearest, id|
                    callback(test_geom, nearest, id).min(nearest));

            let sub_trees: SmallVec<[_; 8]> = sub_trees.collect();
            let sub_tests = test_geom.subdivide();

            for &i in test_geom.test_order().as_ref() {
                nearest = Self::test_impl(
                    sub_trees[i],
                    sub_cells.as_ref()[i],
                    &sub_tests.as_ref()[i],
                    nearest,
                    max_depth,
                    callback);
            }

            nearest
        } else {
            tree.iter()
                .map(|(_, id)| *id)
                .fold(nearest, |nearest, id|
                    callback(test_geom, nearest, id).min(nearest))
        }
    }

    /// Run a single test on some geometry
    /// 
    /// This occurs by repeatedly subdividing both this `Layer`'s index-ID list and the provided
    /// `test_geom`, returning any items at a given depth where both the resulting index list
    /// is non-empty and [`TestGeometry::subdivide`] returns a result
    /// 
    /// _note: this method may do an implicit, non-parallel sort; you may call [`par_sort`] prior
    /// to calling this method to perform a parallel sort instead_
    /// 
    /// [`TestGeometry::subdivide`]: trait.TestGeometry.html#tymethod.subdivide
    /// [`par_sort`]: #method.par_sort
    pub fn test<'a, TestGeom>(
        &'a mut self,
        test_geom: &TestGeom,
        max_depth: Option<u32>) -> &'a Vec<ID>
    where
        TestGeom: TestGeometry
    {
        self.sort();

        self.test_results.clear();

        let (tree, _) = &self.tree;
        let results = &mut self.test_results;
        Self::test_impl(
            tree,
            Index::default(),
            test_geom,
            std::f32::INFINITY,
            max_depth,
            &mut |_, nearest, id| {
                results.push(id);
                nearest
            });

        results.sort();
        results.dedup();

        results
    }

    /// A special case of [`test`] for bounding box tests, see [`BoxTestGeometry`]
    /// 
    /// The `system_bounds` provided to this method should, in most cases, be identical to the
    /// `system_bounds` provided to [`extend`]
    /// 
    /// _note: this method may do an implicit, non-parallel sort; you may call [`par_sort`] prior
    /// to calling this method to perform a parallel sort instead_
    /// 
    /// [`test`]: #method.test
    /// [`extend`]: #method.extend
    /// [`par_sort`]: #method.par_sort
    /// [`BoxTestGeometry`]: struct.BoxTestGeometry.html
    pub fn test_box<'a, Point_>(
        &'a mut self,
        system_bounds: Bounds<Point_>,
        test_bounds: Bounds<Point_>,
        max_depth: Option<u32>) -> &'a Vec<ID>
    where
        Point_: EuclideanSpace<Scalar = f32> + Debug,
        Point_::Diff: ElementWise + std::ops::Index<usize, Output = f32> + Debug,
        BoxTestGeometry<Point_>: TestGeometry
    {
        let test_geom = BoxTestGeometry::with_system_bounds(
            system_bounds,
            test_bounds);

        self.test(
            &test_geom,
            max_depth);

        &self.test_results
    }

    /// A special case of [`test`] for ray-testing, see [`RayTestGeometry`]
    /// 
    /// The `system_bounds` provided to this method should, in most cases, be identical to the
    /// `system_bounds` provided to [`extend`]
    /// 
    /// _note: this method may do an implicit, non-parallel sort; you may call [`par_sort`] prior
    /// to calling this method to perform a parallel sort instead_
    /// 
    /// [`test`]: #method.test
    /// [`extend`]: #method.extend
    /// [`par_sort`]: #method.par_sort
    /// [`RayTestGeometry`]: struct.RayTestGeometry.html
    pub fn test_ray<'a, Point_>(
        &'a mut self,
        system_bounds: Bounds<Point_>,
        origin   : Point_,
        direction: Point_::Diff,
        range_min: f32,
        range_max: f32,
        max_depth: Option<u32>) -> &'a Vec<ID>
    where
        Point_: EuclideanSpace<Scalar = f32> + VecDim + Debug,
        Point_::Diff: ElementWise + std::ops::Index<usize, Output = f32> + Debug,
        RayTestGeometry<Point_>: TestGeometry
    {
        let test_geom = RayTestGeometry::with_system_bounds(
            system_bounds,
            origin,
            direction,
            range_min,
            range_max);

        self.test(
            &test_geom,
            max_depth);

        &self.test_results
    }

    /// Run a picking or hit-test operation
    /// 
    /// This is implemented similarly to [`test`], but differs in that it returns only the nearest
    /// result and may stop searching as soon as the nearest result is found
    /// 
    /// _note: this method may do an implicit, non-parallel sort; you may call [`par_sort`] prior
    /// to calling this method to perform a parallel sort instead_
    /// 
    /// [`test`]: #method.test
    /// [`par_sort`]: #method.par_sort
    pub fn pick<TestGeom, GetDist>(
        &mut self,
        test_geom: &TestGeom,
        max_dist: f32,
        max_depth: Option<u32>,
        mut get_dist: GetDist) -> Option<(f32, ID)>
    where
        TestGeom: TestGeometry,
        GetDist: FnMut(&TestGeom, f32, ID) -> f32
    {
        self.sort();

        self.processed.clear();

        let (tree, _) = &self.tree;
        let processed = &mut self.processed;
        let mut result: Option<ID> = None;
        let dist = Self::test_impl(
            tree,
            Index::default(),
            test_geom,
            max_dist,
            max_depth,
            &mut |test_geom, nearest, id| {
                if processed.insert(id) {
                    let dist = get_dist(test_geom, nearest, id);
                    if dist.is_finite() {
                        if dist < nearest {
                            result = Some(id);
                        }
                        dist
                    } else {
                        std::f32::INFINITY
                    }
                } else {
                    std::f32::INFINITY
                }
            });

        result.map(|id| (dist, id))
    }

    /// A special case of [`pick`] for ray-testing, see [`RayTestGeometry`]
    /// 
    /// The `system_bounds` provided to this method should, in most cases, be identical to the
    /// `system_bounds` provided to [`extend`]
    /// 
    /// _note: this method may do an implicit, non-parallel sort; you may call [`par_sort`] prior
    /// to calling this method to perform a parallel sort instead_
    /// 
    /// [`pick`]: #method.pick
    /// [`extend`]: #method.extend
    /// [`par_sort`]: #method.par_sort
    /// [`RayTestGeometry`]: struct.RayTestGeometry.html
    pub fn pick_ray<Point_, GetDist>(
        &mut self,
        system_bounds: Bounds<Point_>,
        origin   : Point_,
        direction: Point_::Diff,
        max_dist: f32,
        max_depth: Option<u32>,
        mut get_dist: GetDist) -> Option<(f32, ID, Point_)>
    where
        Point_: EuclideanSpace<Scalar = f32> + VecDim + Debug,
        Point_::Diff: VectorSpace<Scalar = f32> + ElementWise + std::ops::Index<usize, Output = f32> + Debug,
        RayTestGeometry<Point_>: TestGeometry,
        GetDist: FnMut(&Point_, &Point_::Diff, f32, ID) -> f32
    {
        let test_geom = RayTestGeometry::with_system_bounds(
            system_bounds,
            origin,
            direction,
            0f32,
            max_dist);

        self.pick(&test_geom, max_dist, max_depth, |_, max_dist, id| {
                get_dist(&origin, &direction, max_dist, id)
            })
            .map(|(dist, id)| {
                let point = origin + direction * dist;
                (dist, id, point)
            })
    }

    /// Detects collisions between all objects in the `Layer`
    pub fn scan<'a>(&'a mut self)
        -> &'a Vec<(ID, ID)>
    {
        self.scan_filtered(|_, _| true)
    }

    /// Detects collisions between all objects in the `Layer`, returning only those which pass a user-specified test
    /// 
    /// Collisions are filtered prior to duplicate removal.  This may be faster or slower than filtering
    /// post-duplicate-removal (i.e. by `scan().iter().filter()`) depending on the complexity
    /// of the filter.
    pub fn scan_filtered<'a, F>(&'a mut self, filter: F)
        -> &'a Vec<(ID, ID)>
    where
        F: FnMut(ID, ID) -> bool
    {
        self.sort();
        
        self.collisions.clear();
        self.invalid.clear();

        let (tree, _) = &self.tree;
        Self::scan_impl(tree.as_slice(), &mut self.collisions, filter);

        self.collisions.sort_unstable();
        self.collisions.dedup();

        &self.collisions
    }

    /// [`scan`]: struct.Layer.html#method.scan
    /// Parallel version of [`scan`]
    #[cfg(feature="parallel")]
    pub fn par_scan<'a>(&'a mut self)
        -> &'a Vec<(ID, ID)>
    where
        Index: Send + Sync
    {
        self.par_scan_filtered(|_, _| true)
    }

    /// [`scan_filtered`]: struct.Layer.html#method.scan_filtered
    /// Parallel version of [`scan_filtered`]
    #[cfg(feature="parallel")]
    pub fn par_scan_filtered<'a, F>(&'a mut self, filter: F)
        -> &'a Vec<(ID, ID)>
    where
        Index: Send + Sync,
        F: Copy + Send + Sync + FnMut(ID, ID) -> bool
    {
        self.par_sort();

        self.collisions.clear();
        self.invalid.clear();
        for set in self.collisions_tls.iter_mut() {
            set.borrow_mut().clear();
        }

        self.par_scan_impl(rayon::current_num_threads(), self.tree.0.as_slice(), filter);

        for set in self.collisions_tls.iter_mut() {
            use std::borrow::Borrow;
            let set_: RefMut<Vec<(ID, ID)>> = set.borrow_mut();
            let set__: &Vec<(ID, ID)> = set_.borrow();
            self.collisions.extend(set__.iter());
        }

        self.collisions.par_sort_unstable();
        self.collisions.dedup();

        &self.collisions
    }

    #[cfg(feature="parallel")]
    fn par_scan_impl<F>(&self, threads: usize, tree: &[(Index, ID)], filter: F)
    where
        Index: Send + Sync,
        F: Copy + Send + Sync + FnMut(ID, ID) -> bool
    {
        const SPLIT_THRESHOLD: usize = 64;
        if threads <= 1 || tree.len() <= SPLIT_THRESHOLD {
            let collisions = self.collisions_tls.get_or(|| RefCell::new(Vec::new()));
            Self::scan_impl(tree, collisions.borrow_mut(), filter);
        } else {
            let n = tree.len();
            let mut i = n / 2;
            while i < n {
                let (last, _) = tree[i-1];
                let (next, _) = tree[i];
                if !Index::same_cell_at_depth(last, next, self.min_depth) {
                    break;
                }
                i += 1;
            }
            let (head, tail) = tree.split_at(i);
            rayon::join(
                || self.par_scan_impl(threads >> 1, head, filter),
                || self.par_scan_impl(threads >> 1, tail, filter));
        }
    }

    fn scan_impl<C, F>(tree: &[(Index, ID)], mut collisions: C, mut filter: F)
    where
        C: DerefMut<Target = Vec<(ID, ID)>>,
        F: FnMut(ID, ID) -> bool
    {
        let mut stack: SmallVec<[(Index, ID); 256]> = SmallVec::new();
        for &(index, id) in tree {
            while let Some(&(index_, _)) = stack.last() {
                if index.overlaps(index_) {
                    break;
                }
                stack.pop();
            }
            if stack.iter().any(|&(_, id_)| id == id_) {
                continue;
            }
            for &(_, id_) in &stack {
                if id != id_ && filter(id, id_) {
                    collisions.push((id, id_));
                }
            }
            stack.push((index, id))
        }
    }
}

impl<Index, ID> PartialEq<Self> for Layer<Index, ID>
where
    Index: SpatialIndex,
    ID: ObjectID,
    Bounds<Index::Point>: IndexGenerator<Index>
{
    fn eq(&self, other: &Self) -> bool {
        self.min_depth == other.min_depth &&
        self.tree      == other.tree
    }
}

impl<Index, ID> Eq for Layer<Index, ID>
where
    Index: SpatialIndex,
    ID: ObjectID,
    Bounds<Index::Point>: IndexGenerator<Index>
{}

impl<Index, ID> Clone for Layer<Index, ID>
where
    Index: SpatialIndex,
    ID: ObjectID,
    Bounds<Index::Point>: IndexGenerator<Index>
{
    fn clone(&self) -> Self {
        Layer{
            min_depth: self.min_depth,
            tree: self.tree.clone(),

            // don't bother cloning the contents of temporary buffers
            collisions: Vec::with_capacity(self.collisions.capacity()),
            test_results: Vec::with_capacity(self.test_results.capacity()),
            processed: FxHashSet::default(),
            invalid: Vec::new(),

            #[cfg(feature="parallel")]
            collisions_tls: CachedThreadLocal::new()
        }
    }
}

/// A builder for `Layer`s
#[derive(Default)]
pub struct LayerBuilder {
    min_depth: u32,
    index_capacity: Option<usize>,
    collision_capacity: Option<usize>,
    test_capacity: Option<usize>
}

impl LayerBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a minimum depth for index generation.
    /// 
    /// This parameter is important for parallel processing.  A higher value improves the partitioning of data and
    /// improves workload balancing.  However, it can also create many more indices/object than is necessary.  A
    /// setting which is too high may result in an excessive number of dynamic allocations and duplication of
    /// intermediate collision pairs, ultimately hurting worst-case performance.
    /// 
    /// A value of zero is the safest performance-wise for _single-threaded_ operations.
    /// 
    /// When using multi-threaded methods, try a value that between
    /// _log<sub>4</sub> number_of_processors_ (2D) or
    /// _log<sub>8</sub> number_of_processors_ (3D) and
    /// _&minus;log<sub>2</sub>(max_object_size/system_bounds_size)_
    /// 
    /// __It is generally better to set this too low than too high__
    pub fn with_min_depth(&mut self, depth: u32) -> &mut Self {
        self.min_depth = depth;
        self
    }

    /// Set an _initial_ capacity for the index list.
    pub fn with_index_capacity(&mut self, capacity: usize) -> &mut Self {
        self.index_capacity = Some(capacity);
        self
    }

    /// Set an _initial_ capacity for the collision results list, used by `Layer::scan`.
    pub fn with_collision_capacity(&mut self, capacity: usize) -> &mut Self {
        self.collision_capacity = Some(capacity);
        self
    }

    /// Set an _initial_ capacity for the test results list, used by `Layer::test` and `Layer::pick`.
    pub fn with_test_capacity(&mut self, capacity: usize) -> &mut Self {
        self.test_capacity = Some(capacity);
        self
    }

    pub fn build<Index, ID>(&self) -> Layer<Index, ID>
    where
        Index: SpatialIndex,
        ID: ObjectID,
        Bounds<Index::Point>: IndexGenerator<Index>
    {
        Layer{
            min_depth: self.min_depth,
            tree: (match self.index_capacity {
                    Some(capacity) => Vec::with_capacity(capacity),
                    None => Vec::new()
                }, true),
            collisions: match self.collision_capacity {
                    Some(capacity) => Vec::with_capacity(capacity),
                    None => Vec::new()
                },
            test_results: match self.test_capacity {
                    Some(capacity) => Vec::with_capacity(capacity),
                    None => Vec::new()
                },
            processed: FxHashSet::default(),
            invalid: Vec::new(),
            #[cfg(feature="parallel")]
            collisions_tls: CachedThreadLocal::new()
        }
    }
}