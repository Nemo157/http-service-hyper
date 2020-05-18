#[derive(Debug, Clone)]
pub struct SpawnCompat<T> {
    inner: T,
}

impl<T> SpawnCompat<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T, Fut> hyper::rt::Executor<Fut> for SpawnCompat<T>
where
    T: futures::task::Spawn,
    Fut: core::future::Future<Output = ()> + Send + 'static,
{
    fn execute(&self, future: Fut) {
        self.inner
            .spawn_obj(Box::pin(future).into())
            .expect("hyper does not support fallible executors")
    }
}
