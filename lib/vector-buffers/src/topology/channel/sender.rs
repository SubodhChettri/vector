use std::sync::Arc;

use async_recursion::async_recursion;
use tokio::sync::Mutex;

use super::limited_queue::LimitedSender;
use crate::{
    buffer_usage_data::BufferUsageHandle,
    variants::{
        disk_v1,
        disk_v2::{self, ProductionFilesystem},
    },
    Bufferable, WhenFull,
};

/// Adapter for papering over various sender backends.
#[derive(Clone, Debug)]
pub enum SenderAdapter<T: Bufferable> {
    /// The in-memory channel buffer.
    InMemory(LimitedSender<T>),

    /// The disk v1 buffer.
    DiskV1(disk_v1::Writer<T>),

    /// The disk v2 buffer.
    DiskV2(Arc<Mutex<disk_v2::Writer<T, ProductionFilesystem>>>),
}

impl<T: Bufferable> From<LimitedSender<T>> for SenderAdapter<T> {
    fn from(v: LimitedSender<T>) -> Self {
        Self::InMemory(v)
    }
}

impl<T: Bufferable> From<disk_v1::Writer<T>> for SenderAdapter<T> {
    fn from(v: disk_v1::Writer<T>) -> Self {
        Self::DiskV1(v)
    }
}

impl<T: Bufferable> From<disk_v2::Writer<T, ProductionFilesystem>> for SenderAdapter<T> {
    fn from(v: disk_v2::Writer<T, ProductionFilesystem>) -> Self {
        Self::DiskV2(Arc::new(Mutex::new(v)))
    }
}

impl<T> SenderAdapter<T>
where
    T: Bufferable,
{
    async fn send(&mut self, item: T) -> Result<(), ()> {
        match self {
            Self::InMemory(tx) => tx.send(item).await.map_err(|_| ()),
            Self::DiskV1(writer) => {
                writer.send(item).await;
                Ok(())
            }
            Self::DiskV2(writer) => {
                let mut writer = writer.lock().await;

                if let Err(e) = writer.write_record(item).await {
                    // Can't really do much except panic here. :sweat:
                    panic!("writer hit unrecoverable error during write: {}", e);
                }

                Ok(())
            }
        }
    }

    async fn try_send(&mut self, item: T) -> Result<Option<T>, ()> {
        match self {
            Self::InMemory(tx) => tx
                .try_send(item)
                .map(|()| None)
                .or_else(|e| Ok(Some(e.into_inner()))),
            Self::DiskV1(writer) => Ok(writer.try_send(item)),
            Self::DiskV2(writer) => {
                let mut writer = writer.lock().await;

                match writer.try_write_record(item).await {
                    Ok(item) => match item {
                        None => {
                            if let Err(e) = writer.flush().await {
                                // Can't really do much except panic here. :sweat:
                                panic!("writer hit unrecoverable error during flush: {}", e);
                            }
                            Ok(None)
                        }
                        Some(item) => Ok(Some(item)),
                    },
                    Err(e) => {
                        // Can't really do much except panic here. :sweat:
                        panic!("writer hit unrecoverable error during write: {}", e);
                    }
                }
            }
        }
    }

    async fn flush(&mut self) -> Result<(), ()> {
        match self {
            Self::InMemory(_) => Ok(()),
            Self::DiskV1(writer) => {
                writer.flush();
                Ok(())
            }
            Self::DiskV2(writer) => {
                let mut writer = writer.lock().await;

                if let Err(e) = writer.flush().await {
                    // Can't really do much except panic here. :sweat:
                    panic!("writer hit unrecoverable error during flush: {}", e);
                }

                Ok(())
            }
        }
    }

    pub fn capacity(&self) -> Option<usize> {
        match self {
            Self::InMemory(tx) => Some(tx.available_capacity()),
            Self::DiskV1(_) | Self::DiskV2(_) => None,
        }
    }
}

/// A buffer sender.
///
/// The sender handles sending events into the buffer, as well as the behavior around handling
/// events when the internal channel is full.
///
/// When creating a buffer sender/receiver pair, callers can specify the "when full" behavior of the
/// sender.  This controls how events are handled when the internal channel is full.  Three modes
/// are possible:
/// - block
/// - drop newest
/// - overflow
///
/// In "block" mode, callers are simply forced to wait until the channel has enough capacity to
/// accept the event.  In "drop newest" mode, any event being sent when the channel is full will be
/// dropped and proceed no further. In "overflow" mode, events will be sent to another buffer
/// sender.  Callers can specify the overflow sender to use when constructing their buffers initially.
///
/// TODO: We should eventually rework `BufferSender`/`BufferReceiver` so that they contain a vector
/// of the fields we already have here, but instead of cascading via calling into `overflow`, we'd
/// linearize the nesting instead, so that `BufferSender` would only ever be calling the underlying
/// `SenderAdapter` instances instead... which would let us get rid of the boxing and
/// `#[async_recursion]` stuff.
#[derive(Clone, Debug)]
pub struct BufferSender<T: Bufferable> {
    base: SenderAdapter<T>,
    overflow: Option<Box<BufferSender<T>>>,
    when_full: WhenFull,
    instrumentation: Option<BufferUsageHandle>,
}

impl<T: Bufferable> BufferSender<T> {
    /// Creates a new [`BufferSender`] wrapping the given channel sender.
    pub fn new(base: SenderAdapter<T>, when_full: WhenFull) -> Self {
        Self {
            base,
            overflow: None,
            when_full,
            instrumentation: None,
        }
    }

    /// Creates a new [`BufferSender`] wrapping the given channel sender and overflow sender.
    pub fn with_overflow(base: SenderAdapter<T>, overflow: BufferSender<T>) -> Self {
        Self {
            base,
            overflow: Some(Box::new(overflow)),
            when_full: WhenFull::Overflow,
            instrumentation: None,
        }
    }

    /// Converts this sender into an overflowing sender using the given `BufferSender<T>`.
    ///
    /// Note: this resets the internal state of this sender, and so this should not be called except
    /// when initially constructing `BufferSender<T>`.
    #[cfg(test)]
    pub fn switch_to_overflow(&mut self, overflow: BufferSender<T>) {
        self.overflow = Some(Box::new(overflow));
        self.when_full = WhenFull::Overflow;
    }

    /// Configures this sender to instrument the items passing through it.
    pub fn with_instrumentation(&mut self, handle: BufferUsageHandle) {
        self.instrumentation = Some(handle);
    }
}

impl<T: Bufferable> BufferSender<T> {
    #[cfg(test)]
    pub(crate) fn get_base_ref(&self) -> &SenderAdapter<T> {
        &self.base
    }

    #[cfg(test)]
    pub(crate) fn get_overflow_ref(&self) -> Option<&BufferSender<T>> {
        self.overflow.as_ref().map(AsRef::as_ref)
    }

    #[async_recursion]
    pub async fn send(&mut self, item: T) -> Result<(), ()> {
        let item_sizing = self
            .instrumentation
            .as_ref()
            .map(|_| (item.event_count(), item.size_of()));

        let mut sent_to_base = true;
        let mut was_dropped = false;
        match self.when_full {
            WhenFull::Block => self.base.send(item).await?,
            WhenFull::DropNewest => {
                if self.base.try_send(item).await?.is_some() {
                    was_dropped = true;
                }
            }
            WhenFull::Overflow => {
                if let Some(item) = self.base.try_send(item).await? {
                    sent_to_base = false;
                    self.overflow
                        .as_mut()
                        .expect("overflow must exist")
                        .send(item)
                        .await?;
                }
            }
        };

        if let Some(instrumentation) = self.instrumentation.as_ref() {
            if let Some((item_count, item_size)) = item_sizing {
                if sent_to_base {
                    instrumentation.increment_received_event_count_and_byte_size(
                        item_count as u64,
                        item_size as u64,
                    );
                }

                if was_dropped {
                    instrumentation.try_increment_dropped_event_count(item_count as u64);
                }
            }
        }

        Ok(())
    }

    #[async_recursion]
    pub async fn flush(&mut self) -> Result<(), ()> {
        self.base.flush().await?;
        if let Some(overflow) = self.overflow.as_mut() {
            overflow.flush().await?;
        }

        Ok(())
    }
}
