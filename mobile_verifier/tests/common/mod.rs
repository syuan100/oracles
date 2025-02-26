use file_store::file_sink::{FileSinkClient, Message as SinkMessage};
use helium_proto::{
    services::poc_mobile::{
        mobile_reward_share::Reward as MobileReward, GatewayReward, MobileRewardShare, RadioReward,
        ServiceProviderReward, SpeedtestAvg, SubscriberReward, UnallocatedReward,
    },
    Message,
};
use std::collections::HashMap;
use tokio::{sync::mpsc::error::TryRecvError, time::timeout};

pub type ValidSpMap = HashMap<String, String>;

#[derive(Debug, Clone)]
pub struct MockCarrierServiceClient {
    pub valid_sps: ValidSpMap,
}

pub struct MockFileSinkReceiver {
    pub receiver: tokio::sync::mpsc::Receiver<SinkMessage>,
}

#[allow(dead_code)]
impl MockFileSinkReceiver {
    pub async fn receive(&mut self) -> Option<Vec<u8>> {
        match timeout(seconds(2), self.receiver.recv()).await {
            Ok(Some(SinkMessage::Data(on_write_tx, msg))) => {
                let _ = on_write_tx.send(Ok(()));
                Some(msg)
            }
            Ok(None) => None,
            Err(e) => panic!("timeout while waiting for message1 {:?}", e),
            Ok(Some(unexpected_msg)) => {
                println!("ignoring unexpected msg {:?}", unexpected_msg);
                None
            }
        }
    }

    pub async fn get_all(&mut self) -> Vec<Vec<u8>> {
        let mut buf = Vec::new();
        while let Ok(SinkMessage::Data(on_write_tx, msg)) = self.receiver.try_recv() {
            let _ = on_write_tx.send(Ok(()));
            buf.push(msg);
        }
        buf
    }

    pub fn assert_no_messages(&mut self) {
        let Err(TryRecvError::Empty) = self.receiver.try_recv() else {
            panic!("receiver should have been empty")
        };
    }

    pub async fn receive_speedtest_avg(&mut self) -> SpeedtestAvg {
        match self.receive().await {
            Some(bytes) => {
                SpeedtestAvg::decode(bytes.as_slice()).expect("Not a valid speedtest average")
            }
            None => panic!("failed to receive speedtest average"),
        }
    }

    pub async fn get_all_speedtest_avgs(&mut self) -> Vec<SpeedtestAvg> {
        self.get_all()
            .await
            .into_iter()
            .map(|bytes| {
                SpeedtestAvg::decode(bytes.as_slice()).expect("Not a valid speedtest average")
            })
            .collect()
    }

    pub async fn receive_radio_reward(&mut self) -> RadioReward {
        match self.receive().await {
            Some(bytes) => {
                let mobile_reward = MobileRewardShare::decode(bytes.as_slice())
                    .expect("failed to decode expected radio reward");
                println!("mobile_reward: {:?}", mobile_reward);
                match mobile_reward.reward {
                    Some(MobileReward::RadioReward(r)) => r,
                    _ => panic!("failed to get radio reward"),
                }
            }
            None => panic!("failed to receive radio reward"),
        }
    }

    pub async fn receive_gateway_reward(&mut self) -> GatewayReward {
        match self.receive().await {
            Some(bytes) => {
                let mobile_reward = MobileRewardShare::decode(bytes.as_slice())
                    .expect("failed to decode expected gateway reward");
                println!("mobile_reward: {:?}", mobile_reward);
                match mobile_reward.reward {
                    Some(MobileReward::GatewayReward(r)) => r,
                    _ => panic!("failed to get gateway reward"),
                }
            }
            None => panic!("failed to receive gateway reward"),
        }
    }

    pub async fn receive_service_provider_reward(&mut self) -> ServiceProviderReward {
        match self.receive().await {
            Some(bytes) => {
                let mobile_reward = MobileRewardShare::decode(bytes.as_slice())
                    .expect("failed to decode expected service provider reward");
                println!("mobile_reward: {:?}", mobile_reward);
                match mobile_reward.reward {
                    Some(MobileReward::ServiceProviderReward(r)) => r,
                    _ => panic!("failed to get service provider reward"),
                }
            }
            None => panic!("failed to receive service provider reward"),
        }
    }

    pub async fn receive_subscriber_reward(&mut self) -> SubscriberReward {
        match self.receive().await {
            Some(bytes) => {
                let mobile_reward = MobileRewardShare::decode(bytes.as_slice())
                    .expect("failed to decode expected subscriber reward");
                println!("mobile_reward: {:?}", mobile_reward);
                match mobile_reward.reward {
                    Some(MobileReward::SubscriberReward(r)) => r,
                    _ => panic!("failed to get subscriber reward"),
                }
            }
            None => panic!("failed to receive subscriber reward"),
        }
    }

    pub async fn receive_unallocated_reward(&mut self) -> UnallocatedReward {
        match self.receive().await {
            Some(bytes) => {
                let mobile_reward = MobileRewardShare::decode(bytes.as_slice())
                    .expect("failed to decode expected unallocated reward");
                println!("mobile_reward: {:?}", mobile_reward);
                match mobile_reward.reward {
                    Some(MobileReward::UnallocatedReward(r)) => r,
                    _ => panic!("failed to get unallocated reward"),
                }
            }
            None => panic!("failed to receive unallocated reward"),
        }
    }
}

pub fn create_file_sink() -> (FileSinkClient, MockFileSinkReceiver) {
    let (tx, rx) = tokio::sync::mpsc::channel(20);
    (
        FileSinkClient {
            sender: tx,
            metric: "metric",
        },
        MockFileSinkReceiver { receiver: rx },
    )
}

pub fn seconds(s: u64) -> std::time::Duration {
    std::time::Duration::from_secs(s)
}
