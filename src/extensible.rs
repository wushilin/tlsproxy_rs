use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::RwLock;
use std::task::{Context, Poll};

pub struct Extensible<T> {
    inner:T,
    extensions: Arc<RwLock<HashMap<TypeId, Arc<dyn Any + 'static + Send + Sync>>>>
}

impl<T> AsyncRead for Extensible<T> where T:AsyncRead + Unpin{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for Extensible<T> where T:AsyncWrite + Unpin{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<T> DerefMut for Extensible<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T> Deref for Extensible<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> Extensible<T> {
    pub fn of(what:T) -> Self {
        Extensible {
            inner: what,
            extensions: Default::default()
        }
    }

    pub fn unwrap(self) -> T {
        self.inner
    }

    pub async fn has_extension<ExtType:'static >(&self) -> bool {
        let extensions = self.extensions.read().await;
        let key = TypeId::of::<ExtType>();
        let result = extensions.contains_key(&key);
        return result;
    }
    pub async fn extend<ExtType:'static + Send + Sync>(&self, ext:ExtType) {
        let mut extensions = self.extensions.write().await;
        let key = TypeId::of::<ExtType>();
        extensions.insert(key, Arc::new(ext));
    }

    pub async fn get_extension<ExtType:'static + Send + Sync >(&self) -> Option<Arc<ExtType>> {
        let extensions = self.extensions.read().await;
        let result = extensions.get(&TypeId::of::<ExtType>());
        match result {
            None => {
                return None;
            },
            Some(inner) => {
                let result:Arc<dyn Any + Send + Sync> = Arc::clone(inner);
                let result = downcast_arc::<ExtType>(result);
                return result;
            }
        }
    }

    pub async fn remove_extension<ExtType:'static>(&self) {
        let mut extensions = self.extensions.write().await;
        extensions.remove(&TypeId::of::<ExtType>());
    }
}

fn downcast_arc<T: 'static + Send + Sync>(arc_any: Arc<dyn Any + Send + Sync>) -> Option<Arc<T>> {
    let casted = arc_any.downcast::<T>();
    match casted {
        Ok(casted) => Some(casted),
        Err(_) => {
            None
        }
    }
}