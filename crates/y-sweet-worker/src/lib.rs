#[cfg(feature = "fetch-event")]
use config::Configuration;
use error::{Error, IntoResponse};
use serde_json::{json, Value};
use server_context::ServerContext;
use std::collections::HashMap;
#[cfg(feature = "fetch-event")]
use worker::{event, Env};
use worker::{Date, Request, Response, Result, RouteContext, Router, Url};
use y_sweet_core::{
    api_types::{validate_doc_name, ClientToken, DocCreationRequest, NewDocResponse},
    auth::Authenticator,
    doc_sync::DocWithSyncKv,
    store::StoreError,
};

pub mod config;
pub mod durable_object;
pub mod error;
pub mod r2_store;
pub mod server_context;
pub mod threadless;

const DURABLE_OBJECT: &str = "Y_SWEET";

fn get_time_millis_since_epoch() -> u64 {
    let now = Date::now();
    now.as_millis()
}

pub fn router(
    context: ServerContext,
) -> std::result::Result<Router<'static, ServerContext>, Error> {
    Ok(Router::with_data(context)
        .get("/", |_, _| Response::ok("Y-Sweet!"))
        .get_async("/check_store", check_store_handler)
        .post_async("/doc/new", new_doc_handler)
        .post_async("/doc/:doc_id/auth", auth_doc_handler)
        .get_async("/doc/ws/:doc_id", forward_to_durable_object))
}

#[cfg(feature = "fetch-event")]
#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: worker::Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    let configuration = Configuration::try_from(&env).map_err(|e| e.to_string())?;
    let context = ServerContext::new(configuration, &env);

    let router = router(context);
    let router = match router {
        Ok(router) => router,
        Err(err) => return err.into(),
    };

    router.run(req, env).await
}

fn check_server_token(
    req: &Request,
    auth: Option<&Authenticator>,
) -> std::result::Result<(), Error> {
    if let Some(auth) = auth {
        let auth_header = req
            .headers()
            .get("Authorization")
            .map_err(|_| Error::ExpectedAuthHeader)?;
        let auth_header_val = auth_header.as_deref().ok_or(Error::ExpectedAuthHeader)?;

        if let Some(token) = auth_header_val.strip_prefix("Bearer ") {
            if auth
                .verify_server_token(token, get_time_millis_since_epoch())
                .is_err()
            {
                return Err(Error::BadAuthHeader);
            }
        } else {
            return Err(Error::BadAuthHeader);
        }
    }
    Ok(())
}

async fn new_doc_handler(req: Request, ctx: RouteContext<ServerContext>) -> Result<Response> {
    new_doc(req, ctx).await.into_response()
}

async fn new_doc(
    mut req: Request,
    mut ctx: RouteContext<ServerContext>,
) -> std::result::Result<NewDocResponse, Error> {
    check_server_token(&req, ctx.data.auth()?)?;

    let body = req
        .json::<DocCreationRequest>()
        .await
        .map_err(|_| Error::BadRequest)?;

    let doc_id = body.doc.unwrap_or_else(|| nanoid::nanoid!());

    if !validate_doc_name(&doc_id) {
        return Err(Error::InvalidDocName);
    }

    let store = Some(ctx.data.store());
    let dwskv = DocWithSyncKv::new(&doc_id, store, || {}).await.unwrap();

    dwskv.sync_kv().persist().await.unwrap();

    let response = NewDocResponse { doc: doc_id };

    Ok(response)
}

async fn check_store_handler(req: Request, ctx: RouteContext<ServerContext>) -> Result<Response> {
    check_store(req, ctx).await.into_response()
}

async fn check_store(
    req: Request,
    mut ctx: RouteContext<ServerContext>,
) -> std::result::Result<Value, Error> {
    check_server_token(&req, ctx.data.auth()?)?;

    let store = ctx.data.store();
    let result = store.init().await;

    let result = match result {
        Ok(_) => json!({"ok": true}),
        Err(StoreError::ConnectionError(_)) => json!({"ok": false, "error": "Connection error."}),
        Err(StoreError::BucketDoesNotExist(_)) => {
            json!({"ok": false, "error": "Bucket does not exist."})
        }
        Err(StoreError::NotAuthorized(_)) => json!({"ok": false, "error": "Not authorized."}),
        _ => json!({"ok": false, "error": "Unknown error."}),
    };

    Ok(result)
}

async fn auth_doc_handler(req: Request, ctx: RouteContext<ServerContext>) -> Result<Response> {
    auth_doc(req, ctx).await.into_response()
}

async fn auth_doc(
    req: Request,
    mut ctx: RouteContext<ServerContext>,
) -> std::result::Result<ClientToken, Error> {
    check_server_token(&req, ctx.data.auth()?)?;

    let doc_id = ctx.param("doc_id").unwrap().to_string();

    let store = ctx.data.store();
    if !store
        .exists(&format!("{doc_id}/data.ysweet"))
        .await
        .map_err(|_| Error::UpstreamConnectionError)?
    {
        return Err(Error::NoSuchDocument);
    }

    let token = ctx
        .data
        .auth()?
        .map(|auth| auth.gen_doc_token(&doc_id, get_time_millis_since_epoch()));

    let url = if let Some(url_prefix) = &ctx.data.config.url_prefix {
        let mut parsed = Url::parse(url_prefix).map_err(|_| Error::ConfigurationError {
            field: "url_prefix".to_string(),
            value: url_prefix.to_string(),
        })?;
        match parsed.scheme() {
            "http" => parsed.set_scheme("ws").map_err(|_| Error::InternalError)?,
            "https" => parsed.set_scheme("wss").map_err(|_| Error::InternalError)?,
            _ => {
                return Err(Error::ConfigurationError {
                    field: "url_prefix".to_string(),
                    value: url_prefix.to_string(),
                })
            }
        };
        format!("{parsed}/doc/ws")
    } else {
        let host = req
            .headers()
            .get("Host")
            .map_err(|_| Error::MissingHostHeader)?
            .ok_or(Error::MissingHostHeader)?;

        // X-Forwarded-Proto is a Cloudflare-specific header. It is set to "http" or "https" depending on the request protocol.
        // https://developers.cloudflare.com/fundamentals/get-started/reference/http-request-headers/#x-forwarded-proto
        let use_https = req
            .headers()
            .get("X-Forwarded-Proto")
            .unwrap_or(None)
            .map(|d| d == "https")
            .unwrap_or_default();

        let scheme = if use_https { "wss" } else { "ws" };

        format!("{scheme}://{host}/doc/ws")
    };

    Ok(ClientToken {
        url,
        doc: doc_id.to_string(),
        token,
    })
}

async fn forward_to_durable_object(
    req: Request,
    mut ctx: RouteContext<ServerContext>,
) -> Result<Response> {
    let doc_id = ctx.param("doc_id").unwrap().to_string();

    if let Some(auth) = ctx.data.auth().unwrap() {
        // Read query params.
        let url = req.url()?;
        let query: HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        let result = query.get("token").ok_or(Error::ExpectedClientAuthHeader);
        let token = match result {
            Ok(token) => token,
            Err(e) => return e.into(),
        };
        let result = auth
            .verify_doc_token(token, &doc_id, get_time_millis_since_epoch())
            .map_err(|_| Error::BadClientAuthHeader);
        if let Err(e) = result {
            return e.into();
        }
    }

    let durable_object = ctx.env.durable_object(DURABLE_OBJECT)?;
    let stub = durable_object.id_from_name(&doc_id)?.get_stub()?;

    // Pass server context to durable object.
    let path = req.path();
    let mut req = req.clone_mut()?; // Mutating an incoming request without a clone is a runtime error.
    *req.path_mut()? = path; // Cloning does not clone path (maybe a workers-rs bug?)

    ctx.data.install_on_request(&mut req)?;

    stub.fetch_with_request(req).await
}

struct DocIdPair {
    id: String,
    doc: DocWithSyncKv,
}
