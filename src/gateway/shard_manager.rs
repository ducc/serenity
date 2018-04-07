use futures::{future, Future, Stream, Poll, Sink, StartSend, AsyncSink};
use ::Error;
use std::collections::{VecDeque, HashMap};
use std::rc::Rc;
use std::cell::RefCell;
use std::time::Duration;
use gateway::shard::Shard;
use model::event::{Event, GatewayEvent};
use tokio_core::reactor::Handle;
use tokio_timer::Timer;
use futures::sync::mpsc::{
    unbounded, UnboundedSender, UnboundedReceiver, 
    channel, Sender as MpscSender, Receiver as MpscReceiver,
    SendError,
};
use tungstenite::{Message as TungsteniteMessage, Error as TungsteniteError};

#[derive(Clone, Copy, Debug)]
pub enum ShardingStrategy {
    Autoshard,
    Range(u64, u64, u64),
}

impl ShardingStrategy {
    pub fn auto() -> Self {
        ShardingStrategy::Autoshard
    }

    pub fn multi(count: u64) -> Self {
        ShardingStrategy::Range(0, count, count)
    }

    pub fn simple() -> Self {
        ShardingStrategy::Range(0, 1, 1)
    }

    pub fn range(index: u64, count: u64, total: u64) -> Self {
        ShardingStrategy::Range(index, count, total)
    }
}

impl Default for ShardingStrategy {
    fn default() -> Self {
        ShardingStrategy::Autoshard
    }
}

#[derive(Clone, Debug, Default)]
pub struct ShardManagerOptions {
    pub strategy: ShardingStrategy,
    pub token: Rc<String>,
    pub ws_uri: Rc<String>,
}

pub type WrappedShard = Rc<RefCell<Shard>>;
pub type Message = (WrappedShard, TungsteniteMessage);
pub type MessageStream = UnboundedReceiver<Message>;
type ShardsMap = Rc<RefCell<HashMap<u64, WrappedShard>>>;

pub struct ShardManager {
    pub queue: VecDeque<u64>,
    shards: ShardsMap,
    pub strategy: ShardingStrategy,
    pub token: Rc<String>,
    pub ws_uri: Rc<String>,
    handle: Handle,
    message_stream: Option<MessageStream>,
    queue_sender: MpscSender<u64>,
    queue_receiver: Option<MpscReceiver<u64>>,
    #[allow(dead_code)]
    non_exhaustive: (),
}

impl ShardManager {
    pub fn new(options: ShardManagerOptions, handle: Handle) -> Self {
        // buffer size of 0 as each sender already incremenets buffer by 1 
        let (queue_sender, queue_receiver) = channel(10);

        Self {
            queue: VecDeque::new(),
            shards: Rc::new(RefCell::new(HashMap::new())),
            strategy: options.strategy,
            token: options.token,
            ws_uri: options.ws_uri,
            handle,
            message_stream: None,
            queue_sender,
            queue_receiver: Some(queue_receiver),
            non_exhaustive: (),
        }
    }

    pub fn start(&mut self) -> Box<Future<Item = (), Error = Error>> {
        let (
            shards_index, 
            shards_count, 
            shards_total
        ) = match self.strategy {
            ShardingStrategy::Autoshard => unimplemented!(),
            ShardingStrategy::Range(i, c, t) => (i, c, t),
        };

        let (sender, receiver) = unbounded();
        self.message_stream = Some(receiver);

        for shard_id in shards_index..shards_count {
            trace!("pushing shard id {} to back of queue", &shard_id);
            self.queue.push_back(shard_id);
        }

        let first_shard_id = self.queue.pop_front()
            .expect("shard start queue is empty");
        
        let token = self.token.clone();
        let shards_map = self.shards.clone();
        let handle = self.handle.clone();

        /*let future = start_shard(
            token.clone(),
            first_shard_id,
            shards_total,
            handle.clone(),
            sender.clone(),
        ).map(move |shard| {
            shards_map.borrow_mut().insert(first_shard_id, shard);
        });*/

        //self.handle.spawn(future);

        let future = process_queue(
            self.queue_receiver.take().unwrap(),
            token.clone(),
            shards_total,
            handle.clone(),
            sender.clone(),
            self.shards.clone(),
        );

        self.queue_sender.try_send(first_shard_id).expect("could not send first shard to start");

        self.handle.spawn(future);

        Box::new(future::ok(()))
    }

    pub fn messages(&mut self) -> MessageStream {
        self.message_stream.take().unwrap() 
    }

    pub fn process(&mut self, event: &GatewayEvent) {
        if let GatewayEvent::Dispatch(_, Event::Ready(event)) = event {
            let shard_id = match &event.ready.shard {
                Some(shard) => shard[0],
                None => {
                    error!("ready event has no shard id");
                    return;
                }
            };

            println!("shard id {} has started", &shard_id);

            if let Err(e) = self.queue_sender.try_send(shard_id) {
                error!("could not send shard id to queue mpsc receiver: {:?}", e);
            }
        }
    }
}

fn process_queue(
    queue_receiver: MpscReceiver<u64>,
    token: Rc<String>,
    shards_total: u64,
    handle: Handle,
    sender: UnboundedSender<Message>,
    shards_map: ShardsMap,
) -> impl Future<Item = (), Error = ()> {
    let timer = Timer::default();

    queue_receiver
        .map(move |shard_id| {
            trace!("received message to start shard {}", &shard_id);
            let token = token.clone();
            let handle = handle.clone();
            let sender = sender.clone();
            let shards_map = shards_map.clone();
            let sleep_future = timer.sleep(Duration::from_secs(6));

            sleep_future
                .map_err(|e| error!("Error sleeping before starting next shard: {:?}", e))
                .and_then(move |_| {
                    start_shard(token, shard_id, shards_total, handle.clone(), sender)
                        .map(move |shard| {
                            shards_map.borrow_mut().insert(shard_id.clone(), shard);
                        }) 

                    /*let future = start_shard(token, shard_id, shards_total, handle.clone(), sender)
                        .map(move |shard| {
                            shards_map.borrow_mut().insert(shard_id.clone(), shard);
                        });

                    handle.spawn(future);*/
                })
        })
        .into_future()
        .map(|_| ())
        .map_err(|_| ())
}

fn start_shard(
    token: Rc<String>, 
    shard_id: u64, 
    shards_total: u64, 
    handle: Handle, 
    sender: UnboundedSender<Message>,
) -> impl Future<Item = WrappedShard, Error = ()> {
    Shard::new(token, [shard_id, shards_total], handle.clone())
        .then(move |result| {
            let shard = match result {
                Ok(shard) => Rc::new(RefCell::new(shard)),
                Err(e) => {
                    return future::err(Error::from(e));
                },
             };

            let sink = MessageSink {
                shard: shard.clone(), 
                sender,
            };

            let future = Box::new(shard.borrow_mut()
                .messages()
                .map_err(MessageSinkError::from)
                .forward(sink)
                .map(|_| ())
                .map_err(|e| error!("Error forwarding shard messages to sink: {:?}", e)));

            handle.spawn(future);
            future::ok(shard)
        })
        .map_err(|e| error!("Error starting shard: {:?}", e))
}

pub enum MessageSinkError {
    MpscSend(SendError<Message>),
    Tungstenite(TungsteniteError),
}

impl From<SendError<Message>> for MessageSinkError {
    fn from(e: SendError<Message>) -> Self {
        MessageSinkError::MpscSend(e)
    }
}

impl From<TungsteniteError> for MessageSinkError {
    fn from(e: TungsteniteError) -> Self {
        MessageSinkError::Tungstenite(e)
    }
}

impl ::std::fmt::Debug for MessageSinkError {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        use std::error::Error;

        write!(f, "{}", match *self {
            MessageSinkError::MpscSend(ref err) => err.description(),
            MessageSinkError::Tungstenite(ref err) => err.description(),
        })
    }
}

struct MessageSink {
    shard: WrappedShard,
    sender: UnboundedSender<Message>,
}

impl Sink for MessageSink {
    type SinkItem = TungsteniteMessage;
    type SinkError = MessageSinkError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        Ok(match self.sender.start_send((self.shard.clone(), item))? {
            AsyncSink::NotReady((_, item)) => AsyncSink::NotReady(item),
            AsyncSink::Ready => AsyncSink::Ready,
        })
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.sender.poll_complete()
            .map_err(From::from)
    }
}