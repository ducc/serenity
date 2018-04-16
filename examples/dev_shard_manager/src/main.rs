extern crate futures;
extern crate serenity;
extern crate tokio_core;
extern crate env_logger;

use serenity::{
    gateway::{
        ShardingStrategy,
        ShardManager,
        ShardManagerOptions,
        SimpleReconnectQueue,
    },
    model::event::{
        Event,
        GatewayEvent,
    },
};
use std::{
    env, 
    rc::Rc,
};
use tokio_core::reactor::{Core, Handle};
use futures::{future, Future, Stream};

fn main() {
    env_logger::init().expect("Error initializing env_logger");

    let mut core = Core::new().expect("Error creating event loop");
    let future = try_main(core.handle());

    core.run(future).expect("Error running event loop");
}

fn try_main(handle: Handle) -> Box<Future<Item = (), Error = ()>> {
    let token = env::var("DISCORD_TOKEN")
        .expect("Expected a token in the environment");

    let opts = ShardManagerOptions {
        strategy: ShardingStrategy::multi(4),
        token: Rc::new(token),
        ws_uri: Rc::new(String::from("nothing")),
        queue: SimpleReconnectQueue::new(4),
    }; 

    let mut shard_manager = ShardManager::new(opts, handle.clone());
    let future = shard_manager.start()
        .map_err(|e| println!("Error starting shard manager: {:?}", e));

    handle.spawn(future);

    let future = shard_manager.messages().for_each(move |(shard, message)| {
        let mut shard = shard.borrow_mut();
        
        let event = shard.parse(message)
            .expect("Could not parse shard stream message");

        shard.process(&event);
        shard_manager.process(&event);

        match event {
            GatewayEvent::Dispatch(_, Event::MessageCreate(ev)) => {
                if ev.message.content == "!ping" {
                    println!("Pong!");
                }
            },
            GatewayEvent::Dispatch(_, Event::Ready(_)) => {
                println!("Connected to Discord!");
            },
            _ => {
                // Ignore all other messages.
            },
        }

        future::ok(())
    });

    Box::new(future)
}
