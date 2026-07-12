use std::cmp::Ordering;

pub trait Distance: Copy + PartialOrd {
    fn cmp_total(&self, other: &Self) -> Ordering;
}
impl Distance for f32 {
    #[inline]
    fn cmp_total(&self, o: &Self) -> Ordering {
        self.total_cmp(o)
    }
}
impl Distance for f64 {
    #[inline]
    fn cmp_total(&self, o: &Self) -> Ordering {
        self.total_cmp(o)
    }
}
macro_rules! ord_dist {
    ($($t:ty),*) => { $(impl Distance for $t {
        #[inline] fn cmp_total(&self, o: &Self) -> Ordering { Ord::cmp(self, o) }
    })* };
}
ord_dist!(u8, u16, u32, u64, usize, i8, i16, i32, i64);

pub trait Scalar: Copy {
    fn to_f32(self) -> f32;
}
impl Scalar for f32 {
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self
    }
}
impl Scalar for f64 {
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self as f32
    }
}

pub trait ScalarInt: Copy {
    fn to_f32(self) -> f32;
}
macro_rules! impl_scalarint {
    ($($t:ty),*) => { $(impl ScalarInt for $t {
        #[inline(always)] fn to_f32(self) -> f32 { self as f32 }
    })* };
}
impl_scalarint!(i8, u8, i16, u16, i32);
impl<T: ScalarInt> Scalar for T {
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self.to_f32()
    }
}

pub trait Metric<T> {
    type Out: Distance;
    fn distance(a: &[T], b: &[T]) -> Self::Out;
}
