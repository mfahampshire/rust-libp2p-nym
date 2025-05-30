// Copyright TODO based on the rust libp2p examples check how to smush 2 together / if this is necessary

use futures::stream::StreamExt;
use libp2p::{
    gossipsub,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use libp2p::{Multiaddr, SwarmBuilder};
use libp2p_identity::Keypair;
use log::{info, LevelFilter};
use rust_libp2p_nym::transport::NymTransport;
use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    hash::{Hash, Hasher},
    time::Duration,
};
use tokio::{io, io::AsyncBufReadExt, select};

#[derive(NetworkBehaviour)]
struct MyBehaviour {
    gossipsub: gossipsub::Behaviour,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::formatted_timed_builder()
        .filter_level(LevelFilter::Info)
        .filter_module("rust_libp2p_nym", LevelFilter::Debug)
        .init();

    let local_key = Keypair::generate_ed25519();
    // let local_peer_id = PeerId::from(local_key.public());

    info!("Running `chat` example using NymTransport");
    let client = nym_sdk::mixnet::MixnetClient::connect_new().await?;
    let transport = NymTransport::new(client, local_key.clone()).await?;

    let mut swarm = SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_other_transport(|_| transport)?
        .with_behaviour(|key| {
            // To content-address message, we can take the hash of message and use it as an ID.
            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };

            // Set a custom gossipsub configuration
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10)) // This is set to aid debugging by not cluttering the log space
                .validation_mode(gossipsub::ValidationMode::Strict) // This sets the kind of message validation. The default is Strict (enforce message
                // signing)
                .message_id_fn(message_id_fn) // content-address messages. No two messages of the same content will be propagated.
                .build()
                .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?; // Temporary hack because `build` does not return a proper `std::error::Error`.

            // build a gossipsub network behaviour
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )?;

            Ok(MyBehaviour { gossipsub })
        })?
        .build();

    // Create a Gossipsub topic
    let topic = gossipsub::IdentTopic::new("nym-transport-test");
    // subscribes to our topic
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    // Read full lines from stdin
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    info!("Enter messages via STDIN and they will be sent to connected peers using Gossipsub");

    // Dial the peer identified by the multi-address given as the second
    // command-line argument, if any, else dial self
    if let Some(addr) = std::env::args().nth(1) {
        let remote: Multiaddr = addr.parse()?;
        swarm.dial(remote)?;
        info!("Dialed {addr}")
    }

    // Kick it off
    loop {
        select! {
            Ok(Some(line)) = stdin.next_line() => {
                if let Err(e) = swarm
                    .behaviour_mut().gossipsub
                    .publish(topic.clone(), line.as_bytes()) {
                    info!("Publish error: {e:?}");
                }
            }
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                })) => info!(
                        "Got message: '{}' with id: {id} from peer: {peer_id}",
                        String::from_utf8_lossy(&message.data),
                    ),
                SwarmEvent::NewListenAddr { address, .. } => {
                    info!("Local node is listening on {address}");
                }
                _ => {}
            }
        }
    }
}
