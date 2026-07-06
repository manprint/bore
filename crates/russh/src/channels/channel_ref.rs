use tokio::sync::mpsc::Sender;

use super::WindowSizeRef;
use crate::ChannelMsg;

/// A handle to the [`super::Channel`]'s to be able to transmit messages
/// to it and update it's `window_size`.
#[derive(Debug)]
pub struct ChannelRef {
    pub(super) sender: Sender<ChannelMsg>,
    pub(super) window_size: WindowSizeRef,
}

impl ChannelRef {
    pub fn new(sender: Sender<ChannelMsg>) -> Self {
        Self {
            sender,
            window_size: WindowSizeRef::new(0),
        }
    }

    pub(crate) fn window_size(&self) -> &WindowSizeRef {
        &self.window_size
    }
}

/// The session's channel map holds the only `ChannelRef`; it is dropped
/// exactly when the channel is torn down (peer CHANNEL_CLOSE processed by
/// `ChannelBacklog::close_with`/`drain`, or the whole session ending). A
/// writer (`ChannelTx`, `ChannelWriteHalf::send_bytes`) parked on the window
/// notifier would otherwise never wake — the peer of a closed channel sends
/// no further WINDOW_ADJUST — wedging its task forever (observed as leaked
/// proxied-connection splices in bore's ssh-gateway). Closing here reaches
/// every teardown path with a single hook.
impl Drop for ChannelRef {
    fn drop(&mut self) {
        self.window_size.close();
    }
}

impl std::ops::Deref for ChannelRef {
    type Target = Sender<ChannelMsg>;

    fn deref(&self) -> &Self::Target {
        &self.sender
    }
}
