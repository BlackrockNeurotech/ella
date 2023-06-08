use crate::{
    shape::{stride_offset, NdimMax},
    Axis, Const, Dyn, IntoShape, RemoveAxis, Shape, Tensor, TensorValue,
};

impl<T, S> Tensor<T, S>
where
    T: TensorValue,
    S: Shape,
{
    pub fn as_dyn(&self) -> Tensor<T, Dyn> {
        Tensor::new(
            self.values().clone(),
            self.shape().as_dyn(),
            self.strides().as_dyn(),
        )
    }

    pub fn reshape<I>(&self, shape: I) -> Tensor<T, I::Shape>
    where
        I: IntoShape,
    {
        let shape = shape.into_shape();
        let strides = shape.default_strides();
        assert_eq!(self.shape().size(), shape.size());

        let values = self.clone().to_standard_layout().into_values();

        Tensor::new(values, shape, strides)
    }

    #[inline]
    pub fn flatten(&self) -> Tensor<T, Const<1>> {
        self.reshape(Const([self.shape().size()]))
    }

    pub fn unsqueeze<A>(&self, axis: A) -> Tensor<T, S::Larger>
    where
        A: Into<Axis>,
    {
        let axis = axis.into();
        Tensor::new(
            self.values().clone(),
            self.shape().insert_axis(axis),
            self.strides().insert_axis(axis),
        )
    }

    pub fn swap_axes<A1, A2>(&self, ax1: A1, ax2: A2) -> Self
    where
        A1: Into<Axis>,
        A2: Into<Axis>,
    {
        let mut shape = self.shape().clone();
        let mut strides = self.strides().clone();
        let ax1 = Axis::index(&ax1.into(), &shape);
        let ax2 = Axis::index(&ax2.into(), &shape);

        shape.slice_mut().swap(ax1, ax2);
        strides.slice_mut().swap(ax1, ax2);
        Tensor::new(self.values().clone(), shape, strides)
    }

    pub fn as_shape<S2>(&self) -> crate::Result<Tensor<T, S2>>
    where
        S2: Shape,
    {
        let shape = S2::from_shape(self.shape())?;
        let strides = S2::from_shape(self.strides())?;
        Ok(Tensor::new(self.values().clone(), shape, strides))
    }

    pub fn broadcast_to<I, O>(&self, shape: I) -> crate::Result<Tensor<T, O>>
    where
        O: Shape,
        I: IntoShape<Shape = O>,
    {
        let to: O = shape.into_shape();
        let from = self.shape();
        if to.ndim() < from.ndim() {
            return Err(crate::Error::Broadcast(from.as_dyn(), to.as_dyn()));
        }

        let mut new_stride = to.clone();

        let mut stride_iter = new_stride.slice_mut().iter_mut().rev();
        for ((er, es), dr) in from
            .slice()
            .iter()
            .rev()
            .zip(self.strides().slice().iter().rev())
            .zip(stride_iter.by_ref())
        {
            if *dr == *er {
                *dr = *es;
            } else if *er == 1 {
                *dr = 0;
            } else {
                return Err(crate::Error::Broadcast(from.as_dyn(), to.as_dyn()));
            }
        }

        for dr in stride_iter {
            *dr = 0;
        }
        Ok(Tensor::new(self.values().clone(), to, new_stride))
    }

    pub fn broadcast_with<T2, S2>(
        &self,
        other: &Tensor<T2, S2>,
    ) -> crate::Result<(
        Tensor<T, <S as NdimMax<S2>>::Output>,
        Tensor<T2, <S as NdimMax<S2>>::Output>,
    )>
    where
        T2: TensorValue,
        S2: Shape,
        S: NdimMax<S2>,
    {
        let shape = self
            .shape()
            .broadcast::<S2, <S as NdimMax<S2>>::Output>(other.shape())?;
        let t1 = if shape.slice() == self.shape().slice() {
            self.as_shape::<<S as NdimMax<S2>>::Output>()
        } else {
            self.broadcast_to(shape.clone())
        }?;
        let t2 = if shape.slice() == other.shape().slice() {
            other.as_shape::<<S as NdimMax<S2>>::Output>()
        } else {
            other.broadcast_to(shape.clone())
        }?;
        Ok((t1, t2))
    }

    pub fn invert_axis<A: Into<Axis>>(&self, axis: A) -> Self {
        let axis: Axis = axis.into();
        let ax = axis.index(self.shape());
        let stride = self.strides()[ax] as isize;
        let size = self.shape()[ax];
        let values = if size != 0 {
            self.values()
                .offset(stride_offset(size - 1, stride as usize))
        } else {
            self.values().clone()
        };
        let mut strides = self.strides().clone();
        strides[ax] = (-stride) as usize;
        Tensor::new(values, self.shape().clone(), strides)
    }
}

/// > 1-D shape operations
impl<T, S> Tensor<T, S>
where
    T: TensorValue,
    S: Shape + RemoveAxis,
{
    pub fn squeeze(&self, axis: Axis) -> Tensor<T, S::Smaller> {
        debug_assert!(axis.index(self.shape()) < self.shape().ndim());
        debug_assert!(self.shape()[axis.index(self.shape())] <= 1);

        let shape = self.shape().remove_axis(axis);
        let strides = self.strides().remove_axis(axis);
        Tensor::new(self.values().clone(), shape, strides)
    }
}

/// 2-D shape operations
impl<T> Tensor<T, Const<2>>
where
    T: TensorValue,
{
    pub fn t(&self) -> Self {
        self.swap_axes(0, 1)
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn test_invert_axis() {
        let x = crate::tensor![[1, 2, 3], [4, 5, 6],];
        let ax0_inv = crate::tensor![[4, 5, 6], [1, 2, 3]];
        let ax1_inv = crate::tensor![[3, 2, 1], [6, 5, 4]];
        assert!(
            x.invert_axis(0).eq(&ax0_inv).all(),
            "{:?} != {:?}",
            x,
            ax0_inv
        );
        assert!(
            x.invert_axis(1).eq(&ax1_inv).all(),
            "{:?} != {:?}",
            x,
            ax1_inv
        );
    }
}
