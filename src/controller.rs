use std::future::Future;

use tokio::task::JoinHandle;
use tokio_tree_context::Context;

pub struct Controller {
    inner: Context,
}

impl Default for Controller {
    fn default() -> Self {
        Self::new()
    }
}

impl Controller {
    pub fn new() -> Self {
        return Self {
            inner: Context::new(),
        };
    }

    /// Cancelled controller will be refreshed. It can be reused again since this point
    pub fn cancel(&mut self) {
        let old = std::mem::replace(&mut self.inner, Context::new());
        drop(old);
    }

    pub fn spawn<T>(&mut self, future: T) -> JoinHandle<Option<T::Output>>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        self.inner.spawn(future)
    }

    pub fn child(&mut self) -> Self {
        Self {
            inner: self.inner.new_child_context(),
        }
    }
}
