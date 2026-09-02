use std::error::Error as StdError;
use unified_access::{Access, AccessMut};

pub trait AccessAnyhow<'a, T>: Access<'a, T, Error: StdError + Send + Sync + 'static> {}

pub trait AccessMutAnyhow<'a, T>:
    AccessMut<'a, T, Error: StdError + Send + Sync + 'static>
{
}

// 2. 为所有满足条件的类型自动实现这个聚合 Trait
impl<'a, T, P> AccessAnyhow<'a, T> for P
where
    P: Access<'a, T>,
    P::Error: StdError + Send + Sync + 'static,
{
}

impl<'a, T, P> AccessMutAnyhow<'a, T> for P
where
    P: AccessMut<'a, T>,
    P::Error: StdError + Send + Sync + 'static,
{
}
