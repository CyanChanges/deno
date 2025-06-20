// Copyright 2018-2025 the Deno authors. MIT license.

mod esbuild;
mod externals;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use deno_ast::ModuleSpecifier;
use deno_config::deno_json::TsTypeLib;
use deno_core::error::AnyError;
use deno_core::resolve_url_or_path;
use deno_core::url::Url;
use deno_core::RequestedModuleType;
use deno_error::JsError;
use deno_graph::ModuleError;
use deno_graph::Position;
use deno_resolver::graph::ResolveWithGraphError;
use deno_resolver::graph::ResolveWithGraphOptions;
use deno_resolver::npm::managed::ResolvePkgFolderFromDenoModuleError;
use deno_runtime::deno_permissions::PermissionsContainer;
use deno_semver::npm::NpmPackageReqReference;
use esbuild_client::protocol;
use esbuild_client::protocol::BuildResponse;
use esbuild_client::EsbuildFlags;
use esbuild_client::EsbuildFlagsBuilder;
use esbuild_client::EsbuildService;
use esbuild_client::ProtocolClient;
use indexmap::IndexMap;
use node_resolver::errors::PackageSubpathResolveError;
use node_resolver::NodeResolutionKind;
use node_resolver::ResolutionMode;
use sys_traits::EnvCurrentDir;

use crate::args::BundleFlags;
use crate::args::BundleFormat;
use crate::args::Flags;
use crate::args::PackageHandling;
use crate::args::SourceMapType;
use crate::factory::CliFactory;
use crate::graph_container::MainModuleGraphContainer;
use crate::graph_container::ModuleGraphContainer;
use crate::graph_container::ModuleGraphUpdatePermit;
use crate::module_loader::CliModuleLoader;
use crate::module_loader::CliModuleLoaderError;
use crate::module_loader::EnhancedGraphError;
use crate::module_loader::LoadCodeSourceError;
use crate::module_loader::LoadCodeSourceErrorKind;
use crate::module_loader::LoadPreparedModuleError;
use crate::module_loader::ModuleLoadPreparer;
use crate::module_loader::PrepareModuleLoadOptions;
use crate::node::CliNodeResolver;
use crate::npm::CliNpmResolver;
use crate::resolver::CliResolver;
use crate::sys::CliSys;
use crate::tools::bundle::externals::ExternalsMatcher;

pub async fn bundle(
  mut flags: Arc<Flags>,
  bundle_flags: BundleFlags,
) -> Result<(), AnyError> {
  {
    let flags_mut = Arc::make_mut(&mut flags);
    flags_mut.unstable_config.sloppy_imports = true;
  }
  let factory = CliFactory::from_flags(flags);

  let esbuild_path = ensure_esbuild_downloaded(&factory).await?;

  let resolver = factory.resolver().await?.clone();
  let module_load_preparer = factory.module_load_preparer().await?.clone();
  let root_permissions = factory.root_permissions_container()?;
  let npm_resolver = factory.npm_resolver().await?;
  let node_resolver = factory.node_resolver().await?;
  let cli_options = factory.cli_options()?;
  let module_loader = factory
    .create_module_loader_factory()
    .await?
    .create_cli_module_loader(root_permissions.clone());
  let sys = factory.sys();
  let init_cwd = cli_options.initial_cwd().to_path_buf();
  let module_graph_container =
    factory.main_module_graph_container().await?.clone();

  #[allow(clippy::arc_with_non_send_sync)]
  let plugin_handler = Arc::new(DenoPluginHandler {
    resolver: resolver.clone(),
    module_load_preparer,
    module_graph_container,
    permissions: root_permissions.clone(),
    module_loader: module_loader.clone(),
    externals_matcher: if bundle_flags.external.is_empty() {
      None
    } else {
      Some(ExternalsMatcher::new(&bundle_flags.external, &init_cwd))
    },
  });
  let start = std::time::Instant::now();

  let resolved_entrypoints =
    resolve_entrypoints(&resolver, &init_cwd, &bundle_flags.entrypoints);
  let _ = plugin_handler
    .prepare_module_load(&resolved_entrypoints)
    .await;

  let roots =
    resolve_roots(resolved_entrypoints, sys, npm_resolver, node_resolver);
  let _ = plugin_handler.prepare_module_load(&roots).await;

  let esbuild = EsbuildService::new(
    esbuild_path,
    esbuild::ESBUILD_VERSION,
    plugin_handler.clone(),
  )
  .await
  .unwrap();
  let client = esbuild.client().clone();

  tokio::spawn(async move {
    let res = esbuild.wait_for_exit().await;
    log::warn!("esbuild exited: {:?}", res);
  });

  let response =
    execute_esbuild_req(client, &bundle_flags, &init_cwd, roots).await;
  handle_esbuild_errors_and_warnings(&response, &init_cwd);

  if let Some(stdout) = response.write_to_stdout {
    let stdout = replace_require_shim(&String::from_utf8_lossy(&stdout));
    crate::display::write_to_stdout_ignore_sigpipe(stdout.as_bytes())?;
  } else if response.errors.is_empty() {
    if bundle_flags.output_dir.is_none()
      && std::env::var("NO_DENO_BUNDLE_HACK").is_err()
      && bundle_flags.output_path.is_some()
    {
      let out = bundle_flags.output_path.as_ref().unwrap();
      let contents = std::fs::read_to_string(out).unwrap();
      let contents = replace_require_shim(&contents);
      std::fs::write(out, contents).unwrap();
    }

    log::info!(
      "{}",
      deno_terminal::colors::green(format!(
        "bundled in {}",
        crate::display::human_elapsed(start.elapsed().as_millis()),
      ))
    );
  }

  if !response.errors.is_empty() {
    deno_core::anyhow::bail!("bundling failed");
  }

  Ok(())
}

// TODO(nathanwhit): MASSIVE HACK
// See tests::specs::bundle::requires_node_builtin for why this is needed.
// Without this hack, that test would fail with "Dynamic require of "util" is not supported"
fn replace_require_shim(contents: &str) -> String {
  contents.replace(
    r#"var __require = /* @__PURE__ */ ((x) => typeof require !== "undefined" ? require : typeof Proxy !== "undefined" ? new Proxy(x, {
  get: (a, b) => (typeof require !== "undefined" ? require : a)[b]
}) : x)(function(x) {
  if (typeof require !== "undefined") return require.apply(this, arguments);
  throw Error('Dynamic require of "' + x + '" is not supported');
});"#,
    r#"import { createRequire } from "node:module";
var __require = createRequire(import.meta.url);
"#,
  )
}

fn format_message(
  message: &esbuild_client::protocol::Message,
  current_dir: &Path,
) -> String {
  format!(
    "{}{}{}",
    message.text,
    if message.id.is_empty() {
      String::new()
    } else {
      format!("[{}] ", message.id)
    },
    if let Some(location) = &message.location {
      if !message.text.contains(" at ") {
        format!(
          "\n    at {}:{}:{}",
          deno_path_util::resolve_url_or_path(
            location.file.as_str(),
            current_dir
          )
          .map(|url| deno_terminal::colors::cyan(url.to_string()))
          .unwrap_or(deno_terminal::colors::cyan(location.file.clone())),
          deno_terminal::colors::yellow(location.line),
          deno_terminal::colors::yellow(location.column)
        )
      } else {
        String::new()
      }
    } else {
      String::new()
    }
  )
}
#[derive(Debug, thiserror::Error, JsError)]
#[class(generic)]
enum BundleError {
  #[error(transparent)]
  Resolver(#[from] deno_resolver::graph::ResolveWithGraphError),
  #[error(transparent)]
  Url(#[from] deno_core::url::ParseError),
  #[error(transparent)]
  ResolveNpmPkg(#[from] ResolvePkgFolderFromDenoModuleError),
  #[error(transparent)]
  SubpathResolve(#[from] PackageSubpathResolveError),
  #[error(transparent)]
  PathToUrlError(#[from] deno_path_util::PathToUrlError),
  #[error(transparent)]
  UrlToPathError(#[from] deno_path_util::UrlToFilePathError),
  #[error(transparent)]
  Io(#[from] std::io::Error),
  #[error(transparent)]
  ResolveUrlOrPathError(#[from] deno_path_util::ResolveUrlOrPathError),
  #[error(transparent)]
  PrepareModuleLoad(#[from] crate::module_loader::PrepareModuleLoadError),
  #[error(transparent)]
  ResolveReqWithSubPath(#[from] deno_resolver::npm::ResolveReqWithSubPathError),
  #[error(transparent)]
  PackageReqReferenceParse(
    #[from] deno_semver::package::PackageReqReferenceParseError,
  ),
  #[allow(dead_code)]
  #[error("Http cache error")]
  HttpCache,
}

fn requested_type_from_map(
  map: &IndexMap<String, String>,
) -> RequestedModuleType {
  let type_ = map.get("type").map(|s| s.as_str());
  match type_ {
    Some("json") => RequestedModuleType::Json,
    Some(other) => RequestedModuleType::Other(other.to_string().into()),
    None => RequestedModuleType::None,
  }
}

struct DenoPluginHandler {
  resolver: Arc<CliResolver>,
  module_load_preparer: Arc<ModuleLoadPreparer>,
  module_graph_container: Arc<MainModuleGraphContainer>,
  permissions: PermissionsContainer,
  module_loader: CliModuleLoader<MainModuleGraphContainer>,
  externals_matcher: Option<ExternalsMatcher>,
}

#[async_trait::async_trait(?Send)]
impl esbuild_client::PluginHandler for DenoPluginHandler {
  async fn on_resolve(
    &self,
    args: esbuild_client::OnResolveArgs,
  ) -> Result<Option<esbuild_client::OnResolveResult>, AnyError> {
    log::debug!("{}: {args:?}", deno_terminal::colors::cyan("on_resolve"));
    if let Some(matcher) = &self.externals_matcher {
      if matcher.is_pre_resolve_match(&args.path) {
        return Ok(Some(esbuild_client::OnResolveResult {
          external: Some(true),
          path: Some(args.path),
          plugin_name: Some("deno".to_string()),
          plugin_data: None,
          ..Default::default()
        }));
      }
    }
    let result = self.bundle_resolve(
      &args.path,
      args.importer.as_deref(),
      args.resolve_dir.as_deref(),
      args.kind,
      args.with,
    );

    let result = match result {
      Ok(r) => r,
      Err(e) => {
        return Ok(Some(esbuild_client::OnResolveResult {
          errors: Some(vec![esbuild_client::protocol::PartialMessage {
            id: "myerror".into(),
            plugin_name: "deno".into(),
            text: e.to_string(),
            ..Default::default()
          }]),
          ..Default::default()
        }));
      }
    };

    Ok(result.map(|r| {
      // TODO(nathanwhit): remap the resolved path to be relative
      // to the output file. It will be tricky to figure out which
      // output file this import will end up in. We may have to use the metafile and rewrite at the end
      let is_external = r.starts_with("node:")
        || self
          .externals_matcher
          .as_ref()
          .map(|matcher| matcher.is_post_resolve_match(&r))
          .unwrap_or(false);

      esbuild_client::OnResolveResult {
        namespace: if r.starts_with("jsr:")
          || r.starts_with("https:")
          || r.starts_with("http:")
          || r.starts_with("data:")
        {
          Some("deno".into())
        } else {
          None
        },
        external: Some(is_external),
        path: Some(r),
        plugin_name: Some("deno".to_string()),
        plugin_data: None,
        ..Default::default()
      }
    }))
  }

  async fn on_load(
    &self,
    args: esbuild_client::OnLoadArgs,
  ) -> Result<Option<esbuild_client::OnLoadResult>, AnyError> {
    let result = self
      .bundle_load(&args.path, requested_type_from_map(&args.with))
      .await;
    let result = match result {
      Ok(r) => r,
      Err(e) => {
        if e.is_unsupported_media_type() {
          return Ok(None);
        }
        return Ok(Some(esbuild_client::OnLoadResult {
          errors: Some(vec![esbuild_client::protocol::PartialMessage {
            plugin_name: "deno".into(),
            text: e.to_string(),
            ..Default::default()
          }]),
          plugin_name: Some("deno".to_string()),
          ..Default::default()
        }));
      }
    };
    log::trace!(
      "{}: {:?}",
      deno_terminal::colors::magenta("on_load"),
      result.as_ref().map(|(code, loader)| format!(
        "{}: {:?}",
        String::from_utf8_lossy(code),
        loader
      ))
    );
    if let Some((code, loader)) = result {
      Ok(Some(esbuild_client::OnLoadResult {
        contents: Some(code),
        loader: Some(loader),
        ..Default::default()
      }))
    } else {
      Ok(None)
    }
  }

  async fn on_start(
    &self,
    _args: esbuild_client::OnStartArgs,
  ) -> Result<Option<esbuild_client::OnStartResult>, AnyError> {
    Ok(None)
  }

  async fn on_end(
    &self,
    _args: esbuild_client::OnEndArgs,
  ) -> Result<Option<esbuild_client::OnEndResult>, AnyError> {
    Ok(None)
  }
}

fn import_kind_to_resolution_mode(
  kind: esbuild_client::protocol::ImportKind,
) -> ResolutionMode {
  match kind {
    protocol::ImportKind::EntryPoint
    | protocol::ImportKind::ImportStatement
    | protocol::ImportKind::ComposesFrom
    | protocol::ImportKind::DynamicImport
    | protocol::ImportKind::ImportRule
    | protocol::ImportKind::UrlToken => ResolutionMode::Import,
    protocol::ImportKind::RequireCall
    | protocol::ImportKind::RequireResolve => ResolutionMode::Require,
  }
}

#[derive(Debug, thiserror::Error, deno_error::JsError)]
pub enum BundleLoadError {
  #[class(inherit)]
  #[error(transparent)]
  CliModuleLoader(#[from] CliModuleLoaderError),
  #[class(inherit)]
  #[error(transparent)]
  ResolveUrlOrPath(#[from] deno_path_util::ResolveUrlOrPathError),
  #[class(inherit)]
  #[error(transparent)]
  ResolveWithGraph(#[from] ResolveWithGraphError),
  #[class(generic)]
  #[error("Wasm modules are not implemented in deno bundle.")]
  WasmUnsupported,
}

impl BundleLoadError {
  pub fn is_unsupported_media_type(&self) -> bool {
    match self {
      BundleLoadError::CliModuleLoader(
        CliModuleLoaderError::LoadCodeSource(LoadCodeSourceError(ref e)),
      ) => match &**e {
        LoadCodeSourceErrorKind::LoadPreparedModule(
          LoadPreparedModuleError::Graph(ref e),
        ) => matches!(
          &**e,
          EnhancedGraphError {
            error: ModuleError::UnsupportedMediaType { .. },
            ..
          }
        ),
        _ => false,
      },
      _ => false,
    }
  }
}

impl DenoPluginHandler {
  fn bundle_resolve(
    &self,
    path: &str,
    importer: Option<&str>,
    resolve_dir: Option<&str>,
    kind: esbuild_client::protocol::ImportKind,
    with: IndexMap<String, String>,
  ) -> Result<Option<String>, BundleError> {
    log::debug!(
      "bundle_resolve: {:?} {:?} {:?} {:?} {:?}",
      path,
      importer,
      resolve_dir,
      kind,
      with
    );
    let mut resolve_dir = resolve_dir.unwrap_or("").to_string();
    let resolver = self.resolver.clone();
    if !resolve_dir.ends_with(std::path::MAIN_SEPARATOR) {
      resolve_dir.push(std::path::MAIN_SEPARATOR);
    }
    let resolve_dir_path = Path::new(&resolve_dir);
    let mut referrer =
      resolve_url_or_path(importer.unwrap_or(""), resolve_dir_path)
        .unwrap_or_else(|_| {
          Url::from_directory_path(std::env::current_dir().unwrap()).unwrap()
        });
    if referrer.scheme() == "file" {
      let pth = referrer.to_file_path().unwrap();
      if (pth.is_dir()) && !pth.ends_with(std::path::MAIN_SEPARATOR_STR) {
        referrer.set_path(&format!(
          "{}{}",
          referrer.path(),
          std::path::MAIN_SEPARATOR
        ));
      }
    }

    log::debug!(
      "{}: {} {} {} {:?}",
      deno_terminal::colors::magenta("op_bundle_resolve"),
      path,
      resolve_dir,
      referrer,
      import_kind_to_resolution_mode(kind)
    );

    let graph = self.module_graph_container.graph();
    let result = resolver.resolve_with_graph(
      &graph,
      path,
      &referrer,
      Position::new(0, 0),
      ResolveWithGraphOptions {
        mode: import_kind_to_resolution_mode(kind),
        kind: NodeResolutionKind::Execution,
        maintain_npm_specifiers: false,
      },
    );

    log::debug!(
      "{}: {:?}",
      deno_terminal::colors::cyan("op_bundle_resolve result"),
      result
    );

    match result {
      Ok(specifier) => Ok(Some(file_path_or_url(&specifier)?)),
      Err(e) => {
        log::debug!("{}: {:?}", deno_terminal::colors::red("error"), e);
        Err(BundleError::Resolver(e))
      }
    }
  }

  async fn prepare_module_load(
    &self,
    specifiers: &[ModuleSpecifier],
  ) -> Result<(), AnyError> {
    let mut graph_permit =
      self.module_graph_container.acquire_update_permit().await;
    let graph: &mut deno_graph::ModuleGraph = graph_permit.graph_mut();
    self
      .module_load_preparer
      .prepare_module_load(
        graph,
        specifiers,
        PrepareModuleLoadOptions {
          is_dynamic: false,
          lib: TsTypeLib::default(),
          permissions: self.permissions.clone(),
          ext_overwrite: None,
          allow_unknown_media_types: true,
          skip_graph_roots_validation: true,
        },
      )
      .await?;
    graph_permit.commit();
    Ok(())
  }

  async fn bundle_load(
    &self,
    specifier: &str,
    requested_type: RequestedModuleType,
  ) -> Result<Option<(Vec<u8>, esbuild_client::BuiltinLoader)>, BundleLoadError>
  {
    log::debug!(
      "{}: {:?} {:?}",
      deno_terminal::colors::magenta("bundle_load"),
      specifier,
      requested_type
    );

    let specifier = deno_core::resolve_url_or_path(
      specifier,
      Path::new(""), // should be absolute already, feels kind of hacky though
    )?;
    let (specifier, loader) = if let Some((specifier, loader)) =
      self.specifier_and_type_from_graph(&specifier)?
    {
      (specifier, loader)
    } else {
      log::debug!(
        "{}: no specifier and type from graph for {}",
        deno_terminal::colors::yellow("warn"),
        specifier
      );

      if specifier.scheme() == "data" {
        return Ok(Some((
          specifier.to_string().as_bytes().to_vec(),
          esbuild_client::BuiltinLoader::DataUrl,
        )));
      }

      let (media_type, _) =
        deno_media_type::resolve_media_type_and_charset_from_content_type(
          &specifier, None,
        );
      if media_type == deno_media_type::MediaType::Unknown {
        return Ok(None);
      }
      (specifier, media_type_to_loader(media_type))
    };
    let loaded = self
      .module_loader
      .load_module_source(&specifier, None, requested_type)
      .await?;

    Ok(Some((loaded.code.as_bytes().to_vec(), loader)))
  }

  fn specifier_and_type_from_graph(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Result<
    Option<(ModuleSpecifier, esbuild_client::BuiltinLoader)>,
    BundleLoadError,
  > {
    let graph = self.module_graph_container.graph();
    let Some(module) = graph.get(specifier) else {
      return Ok(None);
    };
    let (specifier, loader) = match module {
      deno_graph::Module::Js(js_module) => (
        js_module.specifier.clone(),
        media_type_to_loader(js_module.media_type),
      ),
      deno_graph::Module::Json(json_module) => (
        json_module.specifier.clone(),
        esbuild_client::BuiltinLoader::Json,
      ),
      deno_graph::Module::Wasm(_) => {
        return Err(BundleLoadError::WasmUnsupported);
      }
      deno_graph::Module::Npm(module) => {
        let url = self.resolver.resolve_npm_nv_ref(
          &module.nv_reference,
          None,
          ResolutionMode::Import,
          NodeResolutionKind::Execution,
        )?;
        let (media_type, _charset) =
          deno_media_type::resolve_media_type_and_charset_from_content_type(
            &url, None,
          );
        (url, media_type_to_loader(media_type))
      }
      deno_graph::Module::Node(_) => {
        return Ok(None);
      }
      deno_graph::Module::External(_) => {
        return Ok(None);
      }
    };
    Ok(Some((specifier, loader)))
  }
}

fn file_path_or_url(
  url: &Url,
) -> Result<String, deno_path_util::UrlToFilePathError> {
  if url.scheme() == "file" {
    Ok(
      deno_path_util::url_to_file_path(url)?
        .to_string_lossy()
        .into(),
    )
  } else {
    Ok(url.to_string())
  }
}

fn media_type_to_loader(
  media_type: deno_media_type::MediaType,
) -> esbuild_client::BuiltinLoader {
  use deno_ast::MediaType::*;
  match media_type {
    JavaScript | Cjs | Mjs | Mts => esbuild_client::BuiltinLoader::Js,
    TypeScript | Cts | Dts | Dmts | Dcts => esbuild_client::BuiltinLoader::Ts,
    Jsx | Tsx => esbuild_client::BuiltinLoader::Jsx,
    Css => esbuild_client::BuiltinLoader::Css,
    Json => esbuild_client::BuiltinLoader::Json,
    SourceMap => esbuild_client::BuiltinLoader::Text,
    Html => esbuild_client::BuiltinLoader::Text,
    Sql => esbuild_client::BuiltinLoader::Text,
    Wasm => esbuild_client::BuiltinLoader::Binary,
    Unknown => esbuild_client::BuiltinLoader::Binary,
    // _ => esbuild_client::BuiltinLoader::External,
  }
}

fn resolve_entrypoints(
  resolver: &CliResolver,
  init_cwd: &Path,
  entrypoints: &[String],
) -> Vec<Url> {
  let entrypoints = entrypoints
    .iter()
    .map(|e| resolve_url_or_path(e, init_cwd).unwrap())
    .collect::<Vec<_>>();

  let init_cwd_url = Url::from_directory_path(init_cwd).unwrap();

  let mut resolved = Vec::with_capacity(entrypoints.len());

  for e in &entrypoints {
    let r = resolver
      .resolve(
        e.as_str(),
        &init_cwd_url,
        Position::new(0, 0),
        ResolutionMode::Import,
        NodeResolutionKind::Execution,
      )
      .unwrap();
    resolved.push(r);
  }
  resolved
}

fn resolve_roots(
  entrypoints: Vec<Url>,
  sys: CliSys,
  npm_resolver: &CliNpmResolver,
  node_resolver: &CliNodeResolver,
) -> Vec<Url> {
  let mut roots = Vec::with_capacity(entrypoints.len());

  for url in entrypoints {
    let root = if let Ok(v) = NpmPackageReqReference::from_specifier(&url) {
      let referrer =
        ModuleSpecifier::from_directory_path(sys.env_current_dir().unwrap())
          .unwrap();
      let package_folder = npm_resolver
        .resolve_pkg_folder_from_deno_module_req(v.req(), &referrer)
        .unwrap();
      let main_module = node_resolver
        .resolve_binary_export(&package_folder, v.sub_path())
        .unwrap();
      Url::from_file_path(&main_module).unwrap()
    } else {
      url
    };
    roots.push(root)
  }

  roots
}

/// Ensure that an Esbuild binary for the current os/arch is downloaded
/// and ready to use and then return path to it.
async fn ensure_esbuild_downloaded(
  factory: &CliFactory,
) -> Result<PathBuf, AnyError> {
  let installer_factory = factory.npm_installer_factory()?;
  let deno_dir = factory.deno_dir()?;
  let npmrc = factory.npmrc()?;
  let npm_registry_info = installer_factory.registry_info_provider()?;
  let resolver_factory = factory.resolver_factory()?;
  let workspace_factory = resolver_factory.workspace_factory();

  let esbuild_path = esbuild::ensure_esbuild(
    deno_dir,
    npmrc,
    npm_registry_info,
    workspace_factory.workspace_npm_link_packages()?,
    installer_factory.tarball_cache()?,
    factory.npm_cache()?,
  )
  .await?;
  Ok(esbuild_path)
}

fn configure_esbuild_flags(bundle_flags: &BundleFlags) -> EsbuildFlags {
  let mut builder = EsbuildFlagsBuilder::default();

  builder
    .bundle(bundle_flags.one_file)
    .minify(bundle_flags.minify)
    .splitting(bundle_flags.code_splitting)
    .external(bundle_flags.external.clone())
    .tree_shaking(true)
    .format(match bundle_flags.format {
      BundleFormat::Esm => esbuild_client::Format::Esm,
      BundleFormat::Cjs => esbuild_client::Format::Cjs,
      BundleFormat::Iife => esbuild_client::Format::Iife,
    })
    .packages(match bundle_flags.packages {
      PackageHandling::External => esbuild_client::PackagesHandling::External,
      PackageHandling::Bundle => esbuild_client::PackagesHandling::Bundle,
    });

  if let Some(sourcemap_type) = bundle_flags.sourcemap {
    builder.sourcemap(match sourcemap_type {
      SourceMapType::Linked => esbuild_client::Sourcemap::Linked,
      SourceMapType::Inline => esbuild_client::Sourcemap::Inline,
      SourceMapType::External => esbuild_client::Sourcemap::External,
    });
  }

  if let Some(outdir) = bundle_flags.output_dir.clone() {
    builder.outdir(outdir);
  } else if let Some(output_path) = bundle_flags.output_path.clone() {
    builder.outfile(output_path);
  }

  match bundle_flags.platform {
    crate::args::BundlePlatform::Browser => {
      builder.platform(esbuild_client::Platform::Browser);
    }
    crate::args::BundlePlatform::Deno => {}
  }

  builder.build().unwrap()
}

async fn execute_esbuild_req(
  client: ProtocolClient,
  bundle_flags: &BundleFlags,
  init_cwd: &Path,
  roots: Vec<Url>,
) -> BuildResponse {
  let flags = configure_esbuild_flags(bundle_flags);
  let entries = roots.into_iter().map(|e| ("".into(), e.into())).collect();

  let msg = protocol::BuildRequest {
    entries,
    key: 0,
    flags: flags.to_flags(),
    write: true,
    stdin_contents: None.into(),
    stdin_resolve_dir: None.into(),
    abs_working_dir: init_cwd.to_string_lossy().to_string(),
    context: false,
    mangle_cache: None,
    node_paths: vec![],
    plugins: Some(vec![protocol::BuildPlugin {
      name: "deno".into(),
      on_start: false,
      on_end: false,
      on_resolve: (vec![protocol::OnResolveSetupOptions {
        id: 0,
        filter: ".*".into(),
        namespace: "".into(),
      }]),
      on_load: vec![protocol::OnLoadSetupOptions {
        id: 0,
        filter: ".*".into(),
        namespace: "".into(),
      }],
    }]),
  };

  client.send_build_request(msg).await.unwrap()
}

fn handle_esbuild_errors_and_warnings(
  response: &BuildResponse,
  init_cwd: &Path,
) {
  for error in &response.errors {
    log::error!(
      "{}: {}",
      deno_terminal::colors::red_bold("error"),
      format_message(error, init_cwd)
    );
  }

  for warning in &response.warnings {
    log::warn!(
      "{}: {}",
      deno_terminal::colors::yellow("bundler warning"),
      format_message(warning, init_cwd)
    );
  }
}
