use super::*;

// Tuples implement arbitrary splitting of pipelines
macro_rules! impl_push_tuple {
    // entry point: list the pairs once
    ($($name:ident $idx:tt),+ $(,)?) => {
        impl_push_tuple!(@rec [] $($name $idx),+);
    };
    (@rec [$($n:ident $i:tt)*]) => {};
    (@rec [$($n:ident $i:tt)*] $name:ident $idx:tt $(, $rn:ident $ri:tt)*) => {
        impl_push_tuple!(@emit $($n $i)* $name $idx);
        impl_push_tuple!(@rec [$($n $i)* $name $idx] $($rn $ri),*);
    };
    (@emit $($name:ident $idx:tt)+) => {
        /// Fan-out: broadcasts each delta to every element in order. The
        /// reader is the tuple of the elements' readers.
        impl<Z: Clone, $($name: Push<Z>),+> Push<Z> for ($($name,)+) {
            type Reader<'tx, Rd: Readable + 'tx> = ($($name::Reader<'tx, Rd>,)+);
            #[inline]
            fn init(&mut self, init: &mut PipelineInitCtx<'_>) {
                $(self.$idx.init(init);)+
            }
            #[inline]
            fn push(&mut self, tx: &mut WriteTx<'_>, data: &Z, delta: isize) {
                $(self.$idx.push(tx, data, delta);)+
            }
            #[inline]
            fn commit(&mut self, tx: &mut WriteTx<'_>) {
                $(self.$idx.commit(tx);)+
            }
            #[inline]
            fn abort(&mut self) {
                $(self.$idx.abort();)+
            }
            #[inline]
            fn reader<'tx, Rd: Readable>(&self, tx: &'tx Rd) -> Self::Reader<'tx, Rd> {
                ($(self.$idx.reader(tx),)+)
            }
        }
    };
}
impl_push_tuple!(
    A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7,
    I 8, J 9, K 10, L 11, M 12, N 13, O 14, P 15,
);
