use ockam::access_control::DenyAll;
use ockam::authenticated_storage::InMemoryStorage;
use ockam::identity::{Identity, TrustEveryonePolicy};
use ockam::{vault::Vault, Context, Result, TcpTransport};

#[ockam::node(access_control = "DenyAll")]
async fn main(ctx: Context) -> Result<()> {
    // Initialize the TCP Transport.
    let tcp = TcpTransport::create(&ctx).await?;

    // Create:
    //   1. A Vault to store our cryptographic keys
    //   2. An Identity to represent this Node
    //   3. A Secure Channel Listener at Worker address - secure_channel_listener
    //      that will wait for requests to start an Authenticated Key Exchange.

    let vault = Vault::create();
    let e = Identities::create(&ctx, vault).await?;
    let storage = InMemoryStorage::new();
    e.create_secure_channel_listener("secure_channel_listener", TrustEveryonePolicy, &storage)
        .await?;

    // Expect first command line argument to be the TCP address of a target TCP server.
    // For example: 127.0.0.1:4002
    //
    // Create a TCP Transport Outlet - at Ockam Worker address "outlet" -
    // that will connect, as a TCP client, to the target TCP server.
    //
    // This Outlet will:
    // 1. Unwrap the payload of any Ockam Routing Message that it receives from an Inlet
    //    and send it as raw TCP data to the target TCP server. First such message from
    //    an Inlet is used to remember the route back the Inlet.
    //
    // 2. Wrap any raw TCP data it receives, from the target TCP server,
    //    as payload of a new Ockam Routing Message. This Ockam Routing Message will have
    //    its onward_route be set to the route to an Inlet that is knows about because of
    //    a previous message from the Inlet.

    let outlet_target = std::env::args().nth(1).expect("no outlet target given");
    tcp.create_outlet("outlet", outlet_target).await?;

    // Create a TCP listener to receive Ockam Routing Messages from other ockam nodes.
    //
    // Use port 4000, unless otherwise specified by second command line argument.

    let port = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "4000".to_string());
    tcp.listen(format!("127.0.0.1:{port}")).await?;

    #[cfg(feature = "debugger")]
    {
        tokio::time::sleep(core::time::Duration::from_secs(1)).await;

        let mut counter = 0;
        while counter < 20 {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open("/tmp/07-outlet.dot")
                .expect("Unable to open file");
            ockam::debugger::generate_graphs(&mut std::io::BufWriter::new(file))
                .expect("Unable to generate graph");

            //ockam::debugger::display_log();
            println!(".");
            counter += 1;
            tokio::time::sleep(core::time::Duration::from_secs(10)).await;
        }
    }

    // We won't call ctx.stop() here,
    // so this program will keep running until you interrupt it with Ctrl-C.
    Ok(())
}
