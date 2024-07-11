use serde::{Deserialize, Serialize};
use short_uuid::short;

use std::{fmt::{self, Display}, sync::atomic::{AtomicUsize, Ordering}};
use lazy_static::lazy_static;

lazy_static! {
    pub static ref ID_GEN: AtomicUsize = AtomicUsize::new(0);
}

fn next() -> usize {
    ID_GEN.fetch_add(1, Ordering::SeqCst)
}


#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct RequestId {
    _seq: usize,
    _uuid: String,
}

impl Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self._uuid, self._seq)
    }
}
impl RequestId {
    pub fn new() -> Self {
        let uuid = short!().to_string();
        RequestId { _seq: next(), _uuid: uuid }
    }

    pub fn id(&self) -> &str {
        &self._uuid
    }

    pub fn seq(&self) -> usize {
        self._seq
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

