use futures::{pin_mut, select};
use futures::{FutureExt, StreamExt};
use log::debug;
use nym_sdk::mixnet::{
    AnonymousSenderTag, IncludedSurbs, MixnetClient, MixnetClientSender, MixnetMessageSender,
};
use nym_sphinx::addressing::clients::Recipient;
use nym_sphinx::receiver::ReconstructedMessage;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::info;

use super::error::Error;
use super::message::*;

/// initialize_mixnet initializes a read/write connection to a Nym Client.
/// It starts a task that listens for inbound messages from the endpoint and writes outbound messages to the endpoint.
pub(crate) async fn initialize_mixnet(
    client: MixnetClient,
    notify_inbound_tx: Option<UnboundedSender<()>>,
) -> Result<
    (
        Recipient,
        UnboundedReceiver<InboundMessage>,
        UnboundedSender<OutboundMessage>,
    ),
    Error,
> {
    let recipient = *client.nym_address();

    // a channel of inbound messages from the mixnet..
    // the transport reads from (listens) to the inbound_rx.
    // TODO: this is probably a DOS vector; we should limit the size of the channel.
    let (inbound_tx, inbound_rx) = unbounded_channel::<InboundMessage>();

    // a channel of outbound messages to be written to the mixnet.
    // the transport writes to outbound_tx.
    let (outbound_tx, mut outbound_rx) = unbounded_channel::<OutboundMessage>();

    let sink = client.split_sender();
    let mut stream = client;

    tokio::task::spawn(async move {
        loop {
            let t1 = check_inbound(&mut stream, &inbound_tx, &notify_inbound_tx).fuse();
            let t2 = check_outbound(&sink, &mut outbound_rx).fuse();

            pin_mut!(t1, t2);

            select! {
                _ = t1 => {},
                _ = t2 => {},
            };
        }
    });

    Ok((recipient, inbound_rx, outbound_tx))
}

async fn check_inbound(
    client: &mut MixnetClient,
    inbound_tx: &UnboundedSender<InboundMessage>,
    notify_inbound_tx: &Option<UnboundedSender<()>>,
) -> Result<(), Error> {
    if let Some(msg) = client.next().await {
        if let Some(notify_tx) = notify_inbound_tx {
            notify_tx
                .send(())
                .map_err(|e| Error::InboundSendFailure(e.to_string()))?;
        }

        handle_inbound(msg, inbound_tx).await?;
    }

    Err(Error::Unimplemented)
}

async fn handle_inbound(
    msg: ReconstructedMessage,
    inbound_tx: &UnboundedSender<InboundMessage>,
) -> Result<(), Error> {
    let sender_tag = msg.sender_tag.clone();

    let data = parse_message_data(&msg.message, sender_tag)?;
    inbound_tx
        .send(data)
        .map_err(|e| Error::InboundSendFailure(e.to_string()))?;
    Ok(())
}

async fn check_outbound(
    mixnet_sender: &MixnetClientSender,
    outbound_rx: &mut UnboundedReceiver<OutboundMessage>,
) -> Result<(), Error> {
    match outbound_rx.recv().await {
        Some(message) => {
            match &message.message {
                Message::TransportMessage(tm) => match &tm.message.message_type {
                    SubstreamMessageType::OpenResponse => {
                        debug!("Outbound OpenResponse: nonce={}, substream={:?}, has_surb={}, has_recipient={}",
                                               tm.nonce, tm.message.substream_id,
                                               message.sender_tag.is_some(), message.recipient.is_some());
                    }
                    SubstreamMessageType::OpenRequest => {
                        debug!("Outbound OpenRequest: nonce={}, substream={:?}, has_surb={}, has_recipient={}",
                                               tm.nonce, tm.message.substream_id,
                                               message.sender_tag.is_some(), message.recipient.is_some());
                    }
                    SubstreamMessageType::Data(_) => {
                        debug!(
                            "Outbound Data nonce={}, substream={:?}",
                            tm.nonce, tm.message.substream_id
                        );
                    }
                    SubstreamMessageType::Close => {
                        debug!(
                            "Outbound Close nonce={}, substream={:?}",
                            tm.nonce, tm.message.substream_id
                        );
                    }
                },
                Message::ConnectionRequest(_) => debug!("OUTBOUND ConnectionRequest"),
                Message::ConnectionResponse(_) => debug!("OUTBOUND ConnectionResponse"),
            }
            match (&message.recipient, &message.sender_tag) {
                (_, Some(sender_tag)) => {
                    // sender_tag for anonymous replies
                    debug!(
                        "writing reply to sender_tag {:?}",
                        sender_tag.to_base58_string()
                    );
                    write_reply_bytes(
                        mixnet_sender,
                        sender_tag.clone(),
                        &message.message.to_bytes(),
                    )
                    .await
                }
                (Some(recipient), None) => {
                    // recipient for initial messages
                    debug!("sending message to recipient {:}", recipient);
                    write_bytes(
                        mixnet_sender,
                        recipient.clone(),
                        &message.message.to_bytes(),
                    )
                    .await
                }
                (None, None) => {
                    debug!("No recipient or sender_tag provided, cannot route messag");
                    return Err(Error::OutboundSendFailure(
                        "No recipient or sender_tag provided, cannot route message".to_string(),
                    ));
                }
            }
        }
        None => Err(Error::RecvFailure),
    }
}

async fn write_bytes(
    mixnet_sender: &MixnetClientSender,
    recipient: Recipient,
    message: &[u8],
) -> Result<(), Error> {
    if let Err(_err) = mixnet_sender
        .send_message(recipient, message, IncludedSurbs::default()) // was IncludedSurbs::ExposeSelfAddress
        .await
    {
        return Err(Error::Unimplemented);
    }
    debug!("wrote message to recipient: {:?}", recipient.to_string());
    Ok(())
}

async fn write_reply_bytes(
    mixnet_sender: &MixnetClientSender,
    sender_tag: AnonymousSenderTag,
    message: &[u8],
) -> Result<(), Error> {
    if let Err(_err) = mixnet_sender.send_reply(sender_tag, message).await {
        return Err(Error::Unimplemented);
    }
    debug!("wrote reply to sender_tag: {:?}", sender_tag.to_string());
    Ok(())
}

#[cfg(test)]
mod test {
    use super::super::message::{
        self, ConnectionId, Message, SubstreamId, SubstreamMessage, SubstreamMessageType,
        TransportMessage,
    };
    use super::super::mixnet::initialize_mixnet;
    use nym_sdk::mixnet::MixnetClient;

    #[tokio::test]
    async fn test_mixnet_poll_inbound_and_outbound() {
        let client = MixnetClient::connect_new().await.unwrap();
        let (self_address, mut inbound_rx, outbound_tx) =
            initialize_mixnet(client, None).await.unwrap();
        let msg_inner = "hello".as_bytes();
        let substream_id = SubstreamId::generate();
        let msg = Message::TransportMessage(TransportMessage {
            nonce: 1, // arbitrary
            id: ConnectionId::generate(),
            message: SubstreamMessage::new_with_data(substream_id.clone(), msg_inner.to_vec()),
        });

        // send a message to ourselves through the mixnet
        let out_msg = message::OutboundMessage {
            message: msg,
            recipient: Some(self_address),
            sender_tag: None,
        };

        outbound_tx.send(out_msg).unwrap();

        // receive the message from ourselves over the mixnet
        let received_msg = inbound_rx.recv().await.unwrap();
        if let Message::TransportMessage(recv_msg) = received_msg.0 {
            assert_eq!(substream_id, recv_msg.message.substream_id);
            if let SubstreamMessageType::Data(data) = recv_msg.message.message_type {
                assert_eq!(msg_inner, data.as_slice());
            } else {
                panic!("expected SubstreamMessage::Data")
            }
        } else {
            panic!("expected Message::TransportMessage")
        }
    }
}
