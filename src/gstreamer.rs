// EasySplash - tool for animated splash screens
// Copyright (C) 2020  O.S. Systems Software LTDA.
//
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::{animation::Animation, message::Message};
use async_std::{
    io,
    os::unix::net::UnixListener,
    prelude::*,
    sync,
    sync::{Arc, Mutex},
    task,
};
use derive_more::{Display, Error, From};
use gst::{prelude::*, MessageView};
use log::{debug, error, trace};

#[derive(Display, From, Error, Debug)]
pub(crate) enum Error {
    #[display(fmt = "No animation parts to play")]
    NoAnimation,

    #[display(transparent)]
    Io(io::Error),

    #[display(transparent)]
    Bool(gst::glib::error::BoolError),

    #[display(transparent)]
    Glib(gst::glib::error::Error),

    #[display(transparent)]
    StateChange(gst::StateChangeError),

    #[display(transparent)]
    ChannelReceiver(sync::RecvError),
}

enum PipelineStatus {
    Continuous,
    Interruptable,
}

pub(crate) async fn play_animation(
    animation: Animation,
    socket: UnixListener,
) -> Result<(), Error> {
    gst::init()?;
    debug!("Using {} as player", gst::version_string());

    let (status_tx, status_rx) = sync::channel::<PipelineStatus>(1);
    let (message_tx, message_rx) = sync::channel::<Message>(1);
    let playbin = gst::ElementFactory::make("playbin", None)?;

    // TODO: We are not yet handling the animation height and width properties.

    // The pipeline is feed by the `feed_pipeline` and the control messages are
    // handled by the `handle_message` future.
    //
    // Any future which finishes, allow the flow to continue.
    feed_pipeline(status_tx, playbin.clone(), animation)
        .race(handle_client_message(message_tx, socket))
        .race(handle_interrupt_message(status_rx, message_rx))
        .await?;

    playbin.set_state(gst::State::Null)?;

    Ok(())
}

async fn feed_pipeline(
    tx: sync::Sender<PipelineStatus>,
    playbin: gst::Element,
    animation: Animation,
) -> Result<(), Error> {
    // Acquire the iterator so we can walk on the animation parts.
    let mut parts = animation.into_iter();

    // Current playing part.
    let mut current_part = parts.next().ok_or(Error::NoAnimation)?;

    // Queue first animation part and ask GStreamer to start playing it.
    playbin.set_property("uri", &current_part.url())?;
    playbin.set_state(gst::State::Playing)?;

    // We need to wait for stream to start and then we can queue the next
    // part. We do that so we have a gapless playback.
    let bus = playbin.get_bus().expect("failed to get pipeline bus");
    let mut messages = bus.stream();
    while let Some(msg) = messages.next().await {
        match msg.view() {
            MessageView::Error(err) => {
                error!("{}", err.get_error());
                break;
            }
            MessageView::Eos(_) => {
                trace!("end of stream message recived, finishing");
                break;
            }
            MessageView::StreamStart(_) => {
                // Notify if current part is interruptable.
                let status = if current_part.is_interruptable() {
                    debug!("animation part is interruptable");
                    PipelineStatus::Interruptable
                } else {
                    debug!("animation part is intended to be played completely");
                    PipelineStatus::Continuous
                };

                tx.send(status).await;

                // If we have more animation parts to play, queue the next.
                if let Some(part) = parts.next() {
                    current_part = part;

                    trace!("video has started, queuing next part");
                    playbin.set_property("uri", &current_part.url())?;
                }
            }
            _ => (),
        };
    }

    Ok(())
}

async fn handle_client_message(
    tx: sync::Sender<Message>,
    socket: UnixListener,
) -> Result<(), Error> {
    while let Some(stream) = socket.incoming().next().await {
        tx.send(Message::from(stream?.bytes().next().await.expect("unexpected EOF")?)).await
    }

    Ok(())
}

async fn handle_interrupt_message(
    status_rx: sync::Receiver<PipelineStatus>,
    message_rx: sync::Receiver<Message>,
) -> Result<(), Error> {
    let interruptable = Arc::new(Mutex::new(false));

    // This future is responsible to monitor the status changes for the pipeline
    // and mark if it is interruptable or not.
    let status_fut = {
        let interruptable = interruptable.clone();
        async move {
            loop {
                match status_rx.recv().await? {
                    PipelineStatus::Continuous => *interruptable.lock().await = false,
                    PipelineStatus::Interruptable => *interruptable.lock().await = true,
                }
            }
        }
    };

    // The client messages are handled in this future and it takes the
    // interruptable status in consideration.
    let message_fut = async move {
        'outter: loop {
            match message_rx.recv().await? {
                Message::Interrupt => loop {
                    if *interruptable.lock().await {
                        break 'outter Ok(());
                    }

                    // The yield is required to avoid the status_fut to starve.
                    task::yield_now().await;
                },
            }
        }
    };

    status_fut.race(message_fut).await
}
