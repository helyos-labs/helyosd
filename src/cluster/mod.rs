pub mod proto {
    tonic::include_proto!("nexa.cluster.v1");
}

pub mod heartbeat;
pub mod server;
pub mod tls;
pub mod token;
pub mod worker;
