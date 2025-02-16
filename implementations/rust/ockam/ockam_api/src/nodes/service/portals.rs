use crate::error::ApiError;
use crate::nodes::connection::Connection;
use crate::nodes::models::portal::{
    CreateInlet, CreateOutlet, InletList, InletStatus, OutletList, OutletStatus,
};
use crate::nodes::registry::{InletInfo, OutletInfo};
use crate::nodes::service::random_alias;
use crate::session::{util, Data, Replacer, Session};
use crate::{actions, resources, DefaultAddress};
use crate::{local_multiaddr_to_route, try_multiaddr_to_addr};
use minicbor::Decoder;
use ockam::compat::tokio::time::timeout;
use ockam::identity::IdentityIdentifier;
use ockam::{Address, AsyncTryClone, Result};

use ockam_abac::Resource;
use ockam_core::api::{Request, Response, ResponseBuilder};
use ockam_core::compat::sync::Arc;
use ockam_core::flow_control::FlowControlPolicy;
use ockam_core::IncomingAccessControl;
use ockam_multiaddr::proto::{Project, Secure, Service};
use ockam_multiaddr::{MultiAddr, Protocol};
use ockam_node::compat::asynchronous::RwLock;
use ockam_node::Context;
use ockam_transport_tcp::{TcpInletOptions, TcpOutletOptions};
use std::collections::BTreeMap;

use super::{NodeManager, NodeManagerWorker};

const INLET_WORKER: &str = "inlet-worker";
const OUTER_CHAN: &str = "outer-chan";

impl NodeManagerWorker {
    pub(super) fn get_inlets<'a>(
        &self,
        req: &Request<'a>,
        inlet_registry: &'a BTreeMap<String, InletInfo>,
    ) -> ResponseBuilder<InletList<'a>> {
        Response::ok(req.id()).body(InletList::new(
            inlet_registry
                .iter()
                .map(|(alias, info)| {
                    InletStatus::new(
                        &info.bind_addr,
                        info.worker_addr.to_string(),
                        alias,
                        None,
                        info.outlet_route.to_string(),
                    )
                })
                .collect(),
        ))
    }

    pub(super) fn get_outlets<'a>(
        &self,
        req: &Request<'_>,
        outlet_registry: &'a BTreeMap<String, OutletInfo>,
    ) -> ResponseBuilder<OutletList<'a>> {
        Response::ok(req.id()).body(OutletList::new(
            outlet_registry
                .iter()
                .map(|(alias, info)| {
                    OutletStatus::new(&info.tcp_addr, info.worker_addr.to_string(), alias, None)
                })
                .collect(),
        ))
    }

    pub(super) async fn create_inlet<'a>(
        &mut self,
        req: &Request<'_>,
        dec: &mut Decoder<'_>,
        ctx: &Context,
    ) -> Result<ResponseBuilder<InletStatus<'a>>> {
        let manager = self.node_manager.clone();
        let mut node_manager = self.node_manager.write().await;
        let rid = req.id();
        let req: CreateInlet = dec.decode()?;

        let listen_addr = req.listen_addr().to_string();
        let alias = req
            .alias()
            .map(|a| a.to_string())
            .unwrap_or_else(random_alias);

        info!("Handling request to create inlet portal");

        debug! {
            listen_addr = %req.listen_addr(),
            outlet_addr = %req.outlet_addr(),
            %alias,
            "Creating inlet portal"
        }

        // The addressing scheme is very flexible. Typically the node connects to
        // the cloud via secure channel and the with another secure channel via
        // forwarder to the actual outlet on the target node. However it is also
        // possible that there is just a single secure channel used to go directly
        // to another node.
        let (outer, rest) = {
            let connection1 =
                Connection::new(ctx, req.outlet_addr()).with_authorized_identity(req.authorized());
            let connection1 = node_manager.connect(connection1).await?;
            if !connection1.secure_channel.is_empty()
                && connection1
                    .suffix
                    .matches(0, &[Service::CODE.into(), Secure::CODE.into()])
            {
                let addr = connection1
                    .secure_channel
                    .clone()
                    .try_with(connection1.suffix.iter().take(2))?;
                let connection2 = Connection::new(ctx, &addr);
                let connection2 = node_manager.connect(connection2).await?;
                (
                    connection1.secure_channel,
                    connection2
                        .secure_channel
                        .try_with(connection1.suffix.iter().skip(2))?,
                )
            } else {
                (
                    MultiAddr::default(),
                    connection1.secure_channel.try_with(&connection1.suffix)?,
                )
            }
        };

        let outlet_route = match local_multiaddr_to_route(&rest) {
            Some(route) => route,
            None => {
                return Ok(Response::bad_request(rid)
                    .body(InletStatus::bad_request("invalid outlet route")))
            }
        };

        let resource = req.alias().map(Resource::new).unwrap_or(resources::INLET);

        let check_credential = node_manager.enable_credential_checks;
        let project_id = if check_credential {
            let pid = req
                .outlet_addr()
                .first()
                .and_then(|p| {
                    if let Some(p) = p.cast::<Project>() {
                        node_manager.projects.get(&*p).map(|info| &*info.id)
                    } else {
                        None
                    }
                })
                .or_else(|| Some(node_manager.trust_context().ok()?.id()));
            if pid.is_none() {
                return Err(ApiError::generic("credential check requires project"));
            }
            pid
        } else {
            None
        };

        let access_control = node_manager
            .access_control(&resource, &actions::HANDLE_MESSAGE, project_id, None)
            .await?;

        let options = TcpInletOptions::new()
            .with_incoming_access_control(access_control.clone())
            .as_consumer(&node_manager.flow_controls);

        let res = node_manager
            .tcp_transport
            .create_inlet(listen_addr.clone(), outlet_route.clone(), options)
            .await;

        Ok(match res {
            Ok((_, worker_addr)) => {
                // TODO: Use better way to store inlets?
                node_manager.registry.inlets.insert(
                    alias.clone(),
                    InletInfo::new(&listen_addr, Some(&worker_addr), &outlet_route),
                );
                if !outer.is_empty() {
                    let mut s = Session::new(without_outlet_address(rest));
                    s.data().put(INLET_WORKER, worker_addr.clone());
                    s.data().put(OUTER_CHAN, outer);
                    let ctx = Arc::new(ctx.async_try_clone().await?);
                    let repl = replacer(
                        manager,
                        s.data(),
                        listen_addr.clone(),
                        req.outlet_addr().clone(),
                        req.authorized(),
                        access_control.clone(),
                        ctx,
                    );
                    s.set_replacer(repl);
                    node_manager.sessions.lock().unwrap().add(s);
                }

                Response::ok(rid).body(InletStatus::new(
                    listen_addr,
                    worker_addr.to_string(),
                    alias,
                    None,
                    outlet_route.to_string(),
                ))
            }
            Err(e) => {
                warn!(to = %req.outlet_addr(), err = %e, "failed to create tcp inlet");
                // TODO: Use better way to store inlets?
                node_manager.registry.inlets.insert(
                    alias.clone(),
                    InletInfo::new(&listen_addr, None, &outlet_route),
                );

                Response::bad_request(rid).body(InletStatus::new(
                    listen_addr,
                    "",
                    alias,
                    Some(e.to_string().into()),
                    outlet_route.to_string(),
                ))
            }
        })
    }

    pub(super) async fn delete_inlet<'a>(
        &mut self,
        req: &Request<'_>,
        alias: &'a str,
    ) -> Result<ResponseBuilder<InletStatus<'a>>> {
        let mut node_manager = self.node_manager.write().await;

        info!(%alias, "Handling request to delete inlet portal");
        if let Some(inlet_to_delete) = node_manager.registry.inlets.remove(alias) {
            debug!(%alias, "Sucessfully removed inlet from node registry");
            let was_stopped = node_manager
                .tcp_transport
                .stop_inlet(inlet_to_delete.worker_addr.clone())
                .await
                .is_ok();
            if was_stopped {
                debug!(%alias, "Successfully stopped inlet");
                Ok(Response::ok(req.id()).body(InletStatus::new(
                    inlet_to_delete.bind_addr,
                    inlet_to_delete.worker_addr.to_string(),
                    alias,
                    None,
                    inlet_to_delete.outlet_route.to_string(),
                )))
            } else {
                error!(%alias, "Failed to remove inlet from node registry");
                Ok(Response::internal_error(req.id()).body(InletStatus::new(
                    inlet_to_delete.bind_addr,
                    inlet_to_delete.worker_addr.to_string(),
                    alias,
                    Some(format!("Failed to remove inlet with alias {alias}").into()),
                    inlet_to_delete.outlet_route.to_string(),
                )))
            }
        } else {
            error!(%alias, "Inlet not found in the node registry");
            Ok(Response::not_found(req.id()).body(InletStatus::new(
                "".to_string(),
                "".to_string(),
                alias,
                Some(format!("Inlet with alias {alias} not found").into()),
                "".to_string(),
            )))
        }
    }

    pub(super) async fn show_inlet<'a>(
        &mut self,
        req: &Request<'_>,
        alias: &'a str,
    ) -> Result<ResponseBuilder<InletStatus<'a>>> {
        let node_manager = self.node_manager.read().await;

        info!(%alias, "Handling request to show inlet portal");
        if let Some(inlet_to_show) = node_manager.registry.inlets.get(alias) {
            debug!(%alias, "Inlet not found in node registry");
            Ok(Response::ok(req.id()).body(InletStatus::new(
                inlet_to_show.bind_addr.to_string(),
                inlet_to_show.worker_addr.to_string(),
                alias,
                None,
                inlet_to_show.outlet_route.to_string(),
            )))
        } else {
            error!(%alias, "Inlet not found in the node registry");
            Ok(Response::not_found(req.id()).body(InletStatus::new(
                "".to_string(),
                "".to_string(),
                alias,
                Some(format!("Inlet with alias {alias} not found").into()),
                "".to_string(),
            )))
        }
    }

    pub(super) async fn create_outlet<'a>(
        &mut self,
        req: &Request<'_>,
        dec: &mut Decoder<'_>,
    ) -> Result<ResponseBuilder<OutletStatus<'a>>> {
        let mut node_manager = self.node_manager.write().await;
        let CreateOutlet {
            tcp_addr,
            worker_addr,
            alias,
            ..
        } = dec.decode()?;
        let tcp_addr = tcp_addr.to_string();
        let resource = alias
            .as_deref()
            .map(Resource::new)
            .unwrap_or(resources::OUTLET);
        let alias = alias.map(|a| a.0.into()).unwrap_or_else(random_alias);

        info!("Handling request to create outlet portal");
        let worker_addr = Address::from(worker_addr.as_ref());

        let check_credential = node_manager.enable_credential_checks;
        let trust_context_id = if check_credential {
            Some(node_manager.trust_context()?.id())
        } else {
            None
        };

        let access_control = node_manager
            .access_control(&resource, &actions::HANDLE_MESSAGE, trust_context_id, None)
            .await?;
        let options = TcpOutletOptions::new().with_incoming_access_control(access_control);

        // Accept messages from the default secure channel listener
        let options = if let Some(flow_control_id) = node_manager
            .flow_controls
            .get_flow_control_with_spawner(&DefaultAddress::SECURE_CHANNEL_LISTENER.into())
        {
            options.as_consumer(
                &node_manager.flow_controls,
                &flow_control_id,
                FlowControlPolicy::SpawnerAllowMultipleMessages,
            )
        } else {
            options
        };

        let res = node_manager
            .tcp_transport
            .create_outlet(worker_addr.clone(), tcp_addr.clone(), options)
            .await;

        Ok(match res {
            Ok(_) => {
                // TODO: Use better way to store outlets?
                node_manager.registry.outlets.insert(
                    alias.clone(),
                    OutletInfo::new(&tcp_addr, Some(&worker_addr)),
                );

                Response::ok(req.id()).body(OutletStatus::new(
                    tcp_addr,
                    worker_addr.to_string(),
                    alias,
                    None,
                ))
            }
            Err(e) => {
                // TODO: Use better way to store outlets?
                node_manager
                    .registry
                    .outlets
                    .insert(alias.clone(), OutletInfo::new(&tcp_addr, None));

                Response::bad_request(req.id()).body(OutletStatus::new(
                    tcp_addr,
                    worker_addr.to_string(),
                    alias,
                    Some(e.to_string().into()),
                ))
            }
        })
    }

    pub(super) async fn delete_outlet<'a>(
        &mut self,
        req: &Request<'_>,
        alias: &'a str,
    ) -> Result<ResponseBuilder<OutletStatus<'a>>> {
        let mut node_manager = self.node_manager.write().await;

        info!(%alias, "Handling request to delete outlet portal");
        if let Some(outlet_to_delete) = node_manager.registry.outlets.remove(alias) {
            debug!(%alias, "Successfully removed outlet from node registry");
            let was_stopped = node_manager
                .tcp_transport
                .stop_outlet(outlet_to_delete.worker_addr.clone())
                .await
                .is_ok();
            if was_stopped {
                debug!(%alias, "Successfully stopped outlet");
                Ok(Response::ok(req.id()).body(OutletStatus::new(
                    outlet_to_delete.tcp_addr,
                    outlet_to_delete.worker_addr.to_string(),
                    alias,
                    None,
                )))
            } else {
                error!(%alias, "Failed to remove outlet from node registry");
                Ok(Response::internal_error(req.id()).body(OutletStatus::new(
                    outlet_to_delete.tcp_addr,
                    outlet_to_delete.worker_addr.to_string(),
                    alias,
                    Some(format!("Failed to remove outlet with alias {alias}").into()),
                )))
            }
        } else {
            error!(%alias, "Outlet not found in the node registry");
            Ok(Response::not_found(req.id()).body(OutletStatus::new(
                "".to_string(),
                "".to_string(),
                alias,
                Some(format!("Outlet with alias {alias} not found").into()),
            )))
        }
    }

    pub(super) async fn show_outlet<'a>(
        &mut self,
        req: &Request<'_>,
        alias: &'a str,
    ) -> Result<ResponseBuilder<OutletStatus<'a>>> {
        let node_manager = self.node_manager.read().await;

        info!(%alias, "Handling request to show outlet portal");
        if let Some(outlet_to_show) = node_manager.registry.outlets.get(alias) {
            debug!(%alias, "Outlet not found in node registry");
            Ok(Response::ok(req.id()).body(OutletStatus::new(
                outlet_to_show.tcp_addr.to_string(),
                outlet_to_show.worker_addr.to_string(),
                alias,
                None,
            )))
        } else {
            error!(%alias, "Outlet not found in the node registry");
            Ok(Response::not_found(req.id()).body(OutletStatus::new(
                "".to_string(),
                "".to_string(),
                alias,
                Some(format!("Outlet with alias {alias} not found").into()),
            )))
        }
    }
}

/// Create a session replacer.
///
/// This returns a function that accepts the previous ping address (e.g.
/// the secure channel worker address) and constructs the whole route
/// again.
fn replacer(
    manager: Arc<RwLock<NodeManager>>,
    data: Data,
    bind: String,
    addr: MultiAddr,
    auth: Option<IdentityIdentifier>,
    access: Arc<dyn IncomingAccessControl>,
    ctx: Arc<Context>,
) -> Replacer {
    Box::new(move |prev| {
        let addr = addr.clone();
        let auth = auth.clone();
        let bind = bind.clone();
        let manager = manager.clone();
        let access = access.clone();
        let data = data.clone();
        let ctx = ctx.clone();
        Box::pin(async move {
            debug!(%prev, %addr, "creating new tcp inlet");
            // The future that recreates the inlet:
            let f = async {
                let prev = try_multiaddr_to_addr(&prev)?;
                let mut this = manager.write().await;
                let timeout = util::MAX_CONNECT_TIME;

                // First the previous secure channel is deleted, and -- if secure
                // channels were nested -- the outer one as well:

                let _ = this.delete_secure_channel(&ctx, &prev).await;
                if let Some(a) = data.get::<MultiAddr>(OUTER_CHAN) {
                    let a = try_multiaddr_to_addr(&a)?;
                    let _ = this.delete_secure_channel(&ctx, &a).await;
                }

                // Now a connection attempt is made:

                let rest = {
                    let connection1 = Connection::new(ctx.as_ref(), &addr)
                        .with_authorized_identity(auth)
                        .with_timeout(timeout);
                    let connection1 = this.connect(connection1).await?;
                    if !connection1.secure_channel.is_empty()
                        && connection1
                            .suffix
                            .matches(0, &[Service::CODE.into(), Secure::CODE.into()])
                    {
                        // Another secure channel needs to be created. The first one
                        // needs to be remembered so it can be cleaned up if this recovery
                        // executes multiple times:
                        data.put(OUTER_CHAN, connection1.secure_channel.clone());

                        let addr = connection1
                            .secure_channel
                            .clone()
                            .try_with(connection1.suffix.iter().take(2))?;
                        let connection2 =
                            Connection::new(ctx.as_ref(), &addr).with_timeout(timeout);
                        let connection2 = this.connect(connection2).await?;
                        connection2
                            .secure_channel
                            .try_with(connection1.suffix.iter().skip(2))?
                    } else {
                        connection1.secure_channel.try_with(&connection1.suffix)?
                    }
                };

                let r = local_multiaddr_to_route(&rest)
                    .ok_or_else(|| ApiError::message(format!("invalid multiaddr: {rest}")))?;

                // The previous inlet worker needs to be stopped:
                if let Some(wa) = data.get::<Address>(INLET_WORKER) {
                    let _ = this.tcp_transport.stop_inlet(wa).await;
                }

                // Finally attempt to create a new inlet using the new route:
                let wa = this
                    .tcp_transport
                    .create_inlet(
                        bind,
                        r,
                        TcpInletOptions::new().with_incoming_access_control(access),
                    )
                    .await?
                    .1;
                data.put(INLET_WORKER, wa);

                Ok(without_outlet_address(rest))
            };

            // The above future is given some limited time to succeed.
            match timeout(util::MAX_RECOVERY_TIME, f).await {
                Err(_) => {
                    warn!(%addr, "timeout creating new tcp inlet");
                    Err(ApiError::generic("timeout"))
                }
                Ok(Err(e)) => {
                    warn!(%addr, err = %e, "error creating new tcp inlet");
                    Err(e)
                }
                Ok(Ok(a)) => Ok(a),
            }
        })
    })
}

fn without_outlet_address(mut addr: MultiAddr) -> MultiAddr {
    if let Some(p) = addr.last() {
        if let Some(a) = p.cast::<Service>() {
            if "outlet" == &*a {
                addr.pop_back();
            }
        }
    }
    addr
}
