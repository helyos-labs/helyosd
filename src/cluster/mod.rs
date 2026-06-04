pub mod proto {
    tonic::include_proto!("helyos.cluster.v1");
}

pub mod heartbeat;
pub mod server;
pub mod tls;
pub mod token;
pub mod worker;
