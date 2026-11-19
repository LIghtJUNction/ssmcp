//! Versioned C ABI used by packaged native capabilities.
//! JSON owns request/result data; Rust futures and shared service handles never
//! cross it. Every asynchronous result is consumed once through finish().
use super::{Call, NativePlugin, PluginFuture};
use crate::registry::Provider;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    any::Any,
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    marker::PhantomData,
    rc::Rc,
    sync::Arc,
};
use zplugin::{Error, OwnedBytes, OwnedTask};

#[zplugin::interface(id = "dev.ssmcp.capability", version = 1)]
pub trait Capability {
    fn catalog(&self) -> zplugin::Result<OwnedBytes>;
    fn start(
        &mut self,
        request_id: u64,
        native_name: &str,
        input: &[u8],
    ) -> zplugin::Result<OwnedTask<()>>;
    fn finish(&mut self, request_id: u64) -> zplugin::Result<OwnedBytes>;
    fn shutdown(&mut self, request_id: u64) -> zplugin::Result<OwnedTask<()>>;
}

type Seed = Arc<dyn Any + Send + Sync>;
thread_local! {
    // Only a linked static factory sees this explicit construction seed. No
    // pointer or Rust value is passed through the C ABI, and no Runtime callback
    // is installed. Each plugin receives its own concrete state allocation.
    static CONSTRUCTOR: RefCell<Option<Seed>> = const { RefCell::new(None) };
}

struct SeedScope;
impl Drop for SeedScope {
    fn drop(&mut self) {
        CONSTRUCTOR.with(|current| *current.borrow_mut() = None);
    }
}

pub(super) fn error(error: Error) -> String {
    format!("native capability ABI failed: {error}")
}

pub(super) fn validate(header: &'static zplugin::abi::Header) -> Result<(), String> {
    // SAFETY: Registration supplies immutable descriptors and code in this
    // executable, which outlives every local instance and task.
    let plugin = unsafe { zplugin::Plugin::from_static(header) }.map_err(error)?;
    plugin.bind(CapabilityApi::SPEC).map_err(error)?;
    Ok(())
}

pub(super) fn instantiate(
    header: &'static zplugin::abi::Header,
    seed: Seed,
) -> Result<CapabilityProxy<'static>, String> {
    // SAFETY: same static registration contract as validate(); the Rc lease
    // keeps every created instance/task tied to its validated descriptor owner.
    let plugin = Rc::new(unsafe { zplugin::Plugin::from_static(header) }.map_err(error)?);
    CONSTRUCTOR.with(|current| {
        let mut current = current.borrow_mut();
        if current.is_some() {
            return Err("nested native plugin construction is not supported".to_owned());
        }
        *current = Some(seed);
        Ok(())
    })?;
    let _scope = SeedScope;
    let instance = zplugin::Plugin::create_owned(&plugin, CapabilityApi::SPEC).map_err(error)?;
    CapabilityApi::wrap(instance).map_err(error)
}

struct Factory<P>(PhantomData<P>);
impl<P: NativePlugin> zplugin::export::Factory for Factory<P> {
    type Object = Adapter<P>;
    fn create() -> zplugin::Result<Self::Object> {
        let seed = CONSTRUCTOR
            .with(|current| current.borrow_mut().take())
            .ok_or(Error::InvalidArgument)?;
        let plugin = seed.downcast::<P>().map_err(|_| Error::InvalidArgument)?;
        Ok(Adapter {
            plugin,
            results: Rc::new(RefCell::new(BTreeMap::new())),
            closed: false,
        })
    }
}

struct Descriptor<P>(PhantomData<P>);
impl<P: NativePlugin> Descriptor<P> {
    const VALUE: zplugin::abi::PluginDescriptor = zplugin::export::Registration::<
        CapabilityExport<Adapter<P>>,
        Factory<P>,
    >::pinned(zplugin::abi::Version {
        major: 1,
        minor: 0,
        patch: 0,
    });
}

pub(super) fn descriptor<P: NativePlugin>() -> &'static zplugin::abi::Header {
    // Promote the complete descriptor, not just its Header projection: zplugin
    // validates the prefix and then reads the containing PluginDescriptor.
    let descriptor: &'static zplugin::abi::PluginDescriptor = &Descriptor::<P>::VALUE;
    &descriptor.header
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema_version: u32,
    plugin_id: String,
    providers: Vec<CatalogProvider>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogProvider {
    provider: String,
    tools: Vec<Value>,
}

pub(super) fn catalog(
    bytes: &[u8],
    id: &str,
    providers: &[Provider],
) -> Result<Vec<(Provider, Vec<Value>)>, String> {
    let catalog: Catalog = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if catalog.schema_version != 1 || catalog.plugin_id != id {
        return Err("native plugin catalog identity or schema changed".into());
    }
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for entry in catalog.providers {
        let provider = providers
            .iter()
            .copied()
            .find(|provider| provider.label() == entry.provider)
            .ok_or("native plugin catalog tried to claim another provider")?;
        if !seen.insert(provider) {
            return Err("native plugin catalog contains a duplicate provider".into());
        }
        result.push((provider, entry.tools));
    }
    // Ownership may include optional providers that are not registered until an
    // explicit load succeeds. Omitting them must not publish an empty catalog
    // that would revoke already accepted lazy-provider registrations.
    Ok(result)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    id: String,
    provider: String,
    generation: u64,
    arguments: Value,
}

pub(super) fn input(call: &Call) -> Result<Vec<u8>, String> {
    #[derive(Serialize)]
    struct BorrowedInput<'a> {
        id: &'a str,
        provider: &'a str,
        generation: u64,
        arguments: &'a Value,
    }
    serde_json::to_vec(&BorrowedInput {
        id: &call.id,
        provider: call.provider.label(),
        generation: call.generation,
        arguments: &call.arguments,
    })
    .map_err(|error| error.to_string())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Outcome {
    schema_version: u32,
    result: Result<Value, String>,
}

pub(super) fn result(bytes: &[u8]) -> Result<Value, String> {
    let outcome: Outcome = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if outcome.schema_version != 1 {
        return Err("unsupported native plugin result schema".into());
    }
    outcome.result
}

enum Slot {
    Pending,
    Complete(Vec<u8>),
}
type Results = Rc<RefCell<BTreeMap<u64, Slot>>>;

struct Ticket {
    id: u64,
    results: Results,
    complete: bool,
}
impl Drop for Ticket {
    fn drop(&mut self) {
        if !self.complete {
            self.results.borrow_mut().remove(&self.id);
        }
    }
}

struct Adapter<P: NativePlugin> {
    plugin: Arc<P>,
    results: Results,
    closed: bool,
}

impl<P: NativePlugin> Adapter<P> {
    fn task(&self, request_id: u64, future: PluginFuture) -> zplugin::Result<OwnedTask<()>> {
        if let Entry::Vacant(slot) = self.results.borrow_mut().entry(request_id) {
            slot.insert(Slot::Pending);
        } else {
            return Err(Error::Busy);
        }
        let mut ticket = Ticket {
            id: request_id,
            results: self.results.clone(),
            complete: false,
        };
        Ok(OwnedTask::new(async move {
            let result = future.await;
            let bytes = serde_json::to_vec(&Outcome {
                schema_version: 1,
                result,
            })
            .map_err(|_| Error::InvalidArgument)?;
            ticket
                .results
                .borrow_mut()
                .insert(request_id, Slot::Complete(bytes));
            ticket.complete = true;
            Ok(())
        }))
    }
}

impl<P: NativePlugin> Capability for Adapter<P> {
    fn catalog(&self) -> zplugin::Result<OwnedBytes> {
        let providers = self
            .plugin
            .catalog()
            .into_iter()
            .map(|(provider, tools)| CatalogProvider {
                provider: provider.label().into(),
                tools,
            })
            .collect();
        serde_json::to_vec(&Catalog {
            schema_version: 1,
            plugin_id: P::ID.into(),
            providers,
        })
        .map(OwnedBytes::from_vec)
        .map_err(|_| Error::InvalidArgument)
    }

    fn start(
        &mut self,
        request_id: u64,
        native_name: &str,
        input: &[u8],
    ) -> zplugin::Result<OwnedTask<()>> {
        if self.closed || self.results.borrow().contains_key(&request_id) {
            return Err(Error::Busy);
        }
        let input: Input = serde_json::from_slice(input).map_err(|_| Error::InvalidArgument)?;
        let provider = self
            .plugin
            .providers()
            .iter()
            .copied()
            .find(|provider| provider.label() == input.provider)
            .ok_or(Error::InvalidArgument)?;
        let future = self.plugin.clone().call(Call {
            id: input.id,
            provider,
            native_name: native_name.into(),
            generation: input.generation,
            arguments: input.arguments,
        });
        self.task(request_id, future)
    }

    fn finish(&mut self, request_id: u64) -> zplugin::Result<OwnedBytes> {
        let mut results = self.results.borrow_mut();
        match results.entry(request_id) {
            Entry::Occupied(entry) if matches!(entry.get(), Slot::Complete(_)) => {
                let Slot::Complete(bytes) = entry.remove() else {
                    unreachable!()
                };
                Ok(OwnedBytes::from_vec(bytes))
            }
            Entry::Occupied(_) => Err(Error::Busy),
            Entry::Vacant(_) => Err(Error::InvalidArgument),
        }
    }

    fn shutdown(&mut self, request_id: u64) -> zplugin::Result<OwnedTask<()>> {
        if self.closed || !self.results.borrow().is_empty() {
            return Err(Error::Busy);
        }
        self.closed = true;
        self.task(request_id, self.plugin.clone().shutdown())
    }
}
