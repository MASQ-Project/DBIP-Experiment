// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use async_channel::Receiver as WSReceiver;
use masq_lib::ui_gateway::MessageBody;
use masq_lib::ui_traffic_converter::UiTrafficConverter;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use workflow_websocket::client::{Message, WebSocket};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClientListenerError {
    Closed,
    Broken(String),
    Timeout,
    UnexpectedPacket,
}

impl ClientListenerError {
    pub fn is_fatal(&self) -> bool {
        match self {
            ClientListenerError::Closed => true,
            ClientListenerError::Broken(_) => true,
            ClientListenerError::Timeout => true,
            ClientListenerError::UnexpectedPacket => false,
        }
    }
}

pub struct ClientListener {
    websocket: WebSocket,
}

impl ClientListener {
    pub fn new(websocket: WebSocket) -> Self {
        Self { websocket }
    }

    pub async fn start(
        self,
        is_closing: Arc<AtomicBool>,
        message_body_tx: UnboundedSender<Result<MessageBody, ClientListenerError>>,
    ) -> ClientListenerHandle {
        let listener_half = self.websocket.receiver_rx().clone();
        let loop_starter =
            ClientListenerEventLoopSpawner::new(listener_half, message_body_tx, is_closing);
        let task_handle = loop_starter.spawn();
        ClientListenerHandle::new(self.websocket, task_handle)
    }
}

pub struct ClientListenerHandle {
    websocket: WebSocket,
    event_loop_join_handle: JoinHandle<()>,
}

impl Drop for ClientListenerHandle {
    fn drop(&mut self) {
        self.shut_down_listener()
    }
}

impl ClientListenerHandle {
    pub fn new(websocket: WebSocket, event_loop_join_handle: JoinHandle<()>) -> Self {
        Self {
            websocket,
            event_loop_join_handle,
        }
    }

    pub async fn send(&self, msg: Message) -> workflow_websocket::client::Result<&WebSocket> {
        self.websocket.post(msg).await
    }

    pub fn close(&self) -> bool {
        todo!();
        //self.talker_half.close();
    }

    pub fn shut_down_listener(&self) {
        self.event_loop_join_handle.abort()
    }
}

struct ClientListenerEventLoopSpawner {
    listener_half: WSReceiver<Message>,
    message_body_tx: UnboundedSender<Result<MessageBody, ClientListenerError>>,
    is_closing: Arc<AtomicBool>,
}

impl ClientListenerEventLoopSpawner {
    pub fn new(
        listener_half: WSReceiver<Message>,
        message_body_tx: UnboundedSender<Result<MessageBody, ClientListenerError>>,
        is_closing: Arc<AtomicBool>,
    ) -> Self {
        Self {
            listener_half,
            message_body_tx,
            is_closing,
        }
    }

    pub fn spawn(self) -> JoinHandle<()> {
        let future = async move {
            loop {
                let received_ws_message = self.listener_half.recv().await;
                let is_closing = self.is_closing.load(Ordering::Relaxed);

                match (received_ws_message, is_closing) {
                    (_, true) => todo!(),
                    (Ok(Message::Text(string)), _) => {
                        match UiTrafficConverter::new_unmarshal(&string) {
                            Ok(body) => match self.message_body_tx.send(Ok(body.clone())) {
                                Ok(_) => (),
                                Err(_) => break,
                            },
                            Err(_) => match self
                                .message_body_tx
                                .send(Err(ClientListenerError::UnexpectedPacket))
                            {
                                Ok(_) => (),
                                Err(_) => break,
                            },
                        }
                    }
                    (Ok(Message::Open), _) => {
                        // Dropping, it doesn't say anything but what we already know
                    }
                    (Ok(Message::Close), _) => {
                        let _ = self.message_body_tx.send(Err(ClientListenerError::Closed));
                        break;
                    }
                    (Ok(_unexpected), _) => {
                        match self
                            .message_body_tx
                            .send(Err(ClientListenerError::UnexpectedPacket))
                        {
                            Ok(_) => (),
                            Err(_) => break,
                        }
                    }
                    (Err(error), _) => {
                        let _ = self
                            .message_body_tx
                            .send(Err(ClientListenerError::Broken(format!("{:?}", error))));
                        break;
                    }
                }
            }
        };

        tokio::task::spawn(future)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mocks::{make_websocket, websocket_utils};
    use async_channel::{unbounded, Sender};
    use masq_lib::messages::ToMessageBody;
    use masq_lib::messages::{UiShutdownRequest, UiShutdownResponse};
    use masq_lib::test_utils::mock_websockets_server::MockWebSocketsServer;
    use masq_lib::utils::find_free_port;
    use std::time::{Duration, SystemTime};
    use tokio::sync::mpsc::unbounded_channel;
    use workflow_websocket::client::{Ack, Message as ClientMessage};
    use workflow_websocket::server::Message as ServerMessage;

    impl ClientListenerHandle {
        fn is_event_loop_spinning(&self) -> bool {
            !self.event_loop_join_handle.is_finished()
        }
    }

    async fn stimulate_queued_response_from_server(client_listener_handle: &ClientListenerHandle) {
        let message = Message::Text(UiTrafficConverter::new_marshal(UiShutdownRequest {}.tmb(1)));
        client_listener_handle.send(message).await.unwrap();
    }

    #[tokio::test]
    async fn listens_and_passes_data_through() {
        let expected_message = UiShutdownResponse {};
        let port = find_free_port();
        let server =
            MockWebSocketsServer::new(port).queue_response(expected_message.clone().tmb(1));
        let stop_handle = server.start().await;
        let websocket = make_websocket(port);
        let (websocket, talker_half, _) = websocket_utils(port).await;
        let (message_body_tx, mut message_body_rx) = unbounded_channel();
        let mut subject = ClientListener::new(websocket);
        let client_listener_handle = subject
            .start(Arc::new(AtomicBool::new(false)), message_body_tx)
            .await;
        stimulate_queued_response_from_server(&client_listener_handle).await;

        let message_body = message_body_rx.recv().await.unwrap().unwrap();

        assert_eq!(message_body, expected_message.tmb(1));
        let is_spinning = client_listener_handle.is_event_loop_spinning();
        assert_eq!(is_spinning, true);
        let _ = stop_handle.stop();
        wait_for_stop(&client_listener_handle).await;
        let is_spinning = client_listener_handle.is_event_loop_spinning();
        assert_eq!(is_spinning, false);
    }

    #[tokio::test]
    async fn processes_incoming_close_correctly() {
        let port = find_free_port();
        let server = MockWebSocketsServer::new(port)
            .queue_string("close")
            .queue_string("disconnect");
        let stop_handle = server.start().await;
        let (websocket, listener_half, talker_half) = websocket_utils(port).await;
        let (message_body_tx, mut message_body_rx) = unbounded_channel();
        let mut subject = ClientListener::new(websocket);
        let client_listener_handle = subject
            .start(Arc::new(AtomicBool::new(false)), message_body_tx)
            .await;
        let message =
            ClientMessage::Text(UiTrafficConverter::new_marshal(UiShutdownRequest {}.tmb(1)));

        client_listener_handle.send(message).await.unwrap();
        let error = message_body_rx.recv().await.unwrap().err().unwrap();

        assert_eq!(error, ClientListenerError::Closed);
        wait_for_stop(&client_listener_handle).await;
        let is_spinning = client_listener_handle.is_event_loop_spinning();
        assert_eq!(is_spinning, false);
        let _ = stop_handle.stop();
    }

    #[tokio::test]
    async fn processes_broken_connection_correctly() {
        let port = find_free_port();
        let server = MockWebSocketsServer::new(port);
        let stop_handle = server.start().await;
        let (websocket, listener_half, talker_half) = websocket_utils(port).await;
        let listener_half_clone = listener_half.clone();
        let (message_body_tx, mut message_body_rx) = unbounded_channel();
        let mut subject = ClientListener::new(websocket);
        let client_listener_handle = subject
            .start(Arc::new(AtomicBool::new(false)), message_body_tx)
            .await;
        assert!(talker_half.close());

        let error = message_body_rx.recv().await.unwrap().unwrap_err();

        assert_eq!(error, ClientListenerError::Broken("RecvError".to_string()));
        wait_for_stop(&client_listener_handle).await;
        let is_spinning = client_listener_handle.is_event_loop_spinning();
        assert_eq!(is_spinning, false);
    }

    #[tokio::test]
    async fn processes_bad_owned_message_correctly() {
        let port = find_free_port();
        let server =
            MockWebSocketsServer::new(port).queue_owned_message(ServerMessage::Binary(vec![]));
        let stop_handle = server.start().await;
        let websocket = make_websocket(port).await;
        let (message_body_tx, mut message_body_rx) = unbounded_channel();
        let mut subject = ClientListener::new(websocket);
        let client_listener_handle = subject
            .start(Arc::new(AtomicBool::new(false)), message_body_tx)
            .await;
        stimulate_queued_response_from_server(&client_listener_handle).await;

        let error = message_body_rx.recv().await.unwrap().err().unwrap();

        assert_eq!(error, ClientListenerError::UnexpectedPacket);
        let is_spinning = client_listener_handle.is_event_loop_spinning();
        assert_eq!(is_spinning, true);
        let _ = stop_handle.stop();
        wait_for_stop(&client_listener_handle).await;
        let is_spinning = client_listener_handle.is_event_loop_spinning();
        assert_eq!(is_spinning, false);
    }

    #[tokio::test]
    async fn processes_bad_packet_correctly() {
        let port = find_free_port();
        let server = MockWebSocketsServer::new(port).queue_string("booga");
        let stop_handle = server.start().await;
        let websocket = make_websocket(port).await;
        let (message_body_tx, mut message_body_rx) = unbounded_channel();
        let mut subject = ClientListener::new(websocket);
        let client_listener_handle = subject
            .start(Arc::new(AtomicBool::new(false)), message_body_tx)
            .await;
        stimulate_queued_response_from_server(&client_listener_handle).await;

        let error = message_body_rx.recv().await.unwrap().err().unwrap();

        assert_eq!(error, ClientListenerError::UnexpectedPacket);
        let is_running = client_listener_handle.is_event_loop_spinning();
        assert_eq!(is_running, true);
        let _ = stop_handle.stop();
        wait_for_stop(&client_listener_handle).await;
        let is_running = client_listener_handle.is_event_loop_spinning();
        assert_eq!(is_running, false);
    }

    #[tokio::test]
    async fn drop_implementation_works_correctly() {
        let port = find_free_port();
        let server = MockWebSocketsServer::new(port);
        let stop_handle = server.start().await;
        let (websocket, _, _) = websocket_utils(port).await;
        let ref_counting_object = Arc::new(123);
        let cloned = ref_counting_object.clone();
        let join_handle = tokio::task::spawn(async move {
            let cloned = cloned;
            loop {
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        });
        let client_handle = ClientListenerHandle::new(websocket, join_handle);
        let count_before = Arc::strong_count(&ref_counting_object);

        drop(client_handle);

        assert_eq!(count_before, 2);
        while Arc::strong_count(&ref_counting_object) > 1 {
            tokio::time::sleep(Duration::from_millis(10)).await
        }
        let count_after = Arc::strong_count(&ref_counting_object);
        let _ = stop_handle.stop();
    }

    #[test]
    fn client_listener_errors_know_their_own_fatality() {
        assert_eq!(ClientListenerError::Closed.is_fatal(), true);
        assert_eq!(ClientListenerError::Broken("".to_string()).is_fatal(), true);
        assert_eq!(ClientListenerError::Timeout.is_fatal(), true);
        assert_eq!(ClientListenerError::UnexpectedPacket.is_fatal(), false);
    }

    async fn wait_for_stop(listener_handle: &ClientListenerHandle) {
        listener_handle.event_loop_join_handle.abort();
        let mut retries = 100;
        while retries > 0 {
            retries -= 1;
            if !listener_handle.is_event_loop_spinning() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("ClientListener was supposed to stop but didn't");
    }
}
