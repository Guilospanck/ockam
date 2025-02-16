// This node starts a tcp listener and an echoer worker.
// It then runs forever waiting for messages.

use abac_examples::access_control::{AbacUnwrapperWorker, AttributeBasedAccessControl};
use abac_examples::Echoer;

use ockam::abac::{self, AbacAttributeStorage, AbacPolicyStorage, Action, Resource, Subject};
use ockam::access_control::{AllowedTransport, LocalOriginOnly};
use ockam::{Context, Result, TcpTransport, WorkerBuilder, TCP};

#[ockam::node(access_control = "LocalOriginOnly")]
async fn main(ctx: Context) -> Result<()> {
    // Create an in-memory attribute store
    let mem = abac::mem::Memory::new();

    // Set up some subjects with attributes
    mem.set_subject_attributes(
        Subject::from(0x0000_0000_0000_0001),
        [
            ("role".into(), abac::string("reader")),
            ("project".into(), abac::string("green")),
        ]
            .into(),
    )
        .await?;
    mem.set_subject_attributes(
        Subject::from(0x0000_0000_0000_0002),
        [
            ("role".into(), abac::string("writer")),
            ("project".into(), abac::string("green")),
        ]
            .into(),
    )
        .await?;
    mem.set_subject_attributes(
        Subject::from(0x0000_0000_0000_0003),
        [
            ("role".into(), abac::string("writer")),
            ("project".into(), abac::string("blue")),
        ]
            .into(),
    )
        .await?;

    // Set up some conditionals on attributes
    let project_green = abac::eq("project", abac::string("green"));
    let project_blue = abac::eq("project", abac::string("blue"));
    let role_reader = abac::eq("role", abac::string("reader"));
    let role_writer = abac::eq("role", abac::string("writer"));

    // Set some policies for actions on resources
    mem.set_policy(
        Resource::from("/project/green/1234"),
        Action::from("read"),
        &project_green.and(&role_reader.or(&role_writer)),
    )
        .await?;
    mem.set_policy(
        Resource::from("/project/green/1234"),
        Action::from("write"),
        &project_green.and(&role_writer),
    )
        .await?;
    mem.set_policy(
        Resource::from("/project/blue/5678"),
        Action::from("write"),
        &project_blue.and(&role_writer),
    )
        .await?;

    // Initialize the TCP Transport.
    let tcp = TcpTransport::create(&ctx).await?;

    // Create a TCP listener and wait for incoming connections.
    tcp.listen("127.0.0.1:4000").await?;

    // Start an abac unwrapper worker
    WorkerBuilder::with_access_control(
        AllowedTransport::single(TCP),
        "abac_unwrapper",
        AbacUnwrapperWorker,
    )
        .start(&ctx)
        .await?;

    // Create an echoer worker
    WorkerBuilder::with_access_control(AttributeBasedAccessControl::new(mem), "echoer", Echoer)
        .start(&ctx)
        .await?;

    // Don't call node.stop() here so this node runs forever.
    Ok(())
}
