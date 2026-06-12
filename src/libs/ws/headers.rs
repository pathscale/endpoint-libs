use convert_case::Case;
use convert_case::Casing;
use eyre::{Context, ContextCompat, Result, bail};
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::*;

use crate::libs::error_code::ErrorCode;
use crate::libs::handler::Response;
use crate::libs::toolbox::{ArcToolbox, CustomError, RequestContext, Toolbox};
use crate::model::EndpointSchema;
use crate::model::Type;

use super::{WsConnection, WsRequest, request_error_to_resp};

pub trait AuthController: Sync + Send {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<()>>;
}

pub struct SimpleAuthController;

impl AuthController for SimpleAuthController {
    fn auth(
        self: Arc<Self>,
        _toolbox: &ArcToolbox,
        _header: String,
        _conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<()>> {
        async move { Ok(()) }.boxed()
    }
}

pub trait SubAuthController: Sync + Send {
    type Request: WsRequest + 'static;
    type Error: Into<CustomError> + 'static;

    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        req: Self::Request,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, AuthResponse<Self::Request, Self::Error>>;
}

#[allow(type_alias_bounds)]
pub type AuthResponse<T: WsRequest, E = CustomError> = Response<T, E>;

#[doc(hidden)]
pub trait SubAuthControllerErased: Sync + Send {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, ()>;
}

impl<T: SubAuthController + 'static> SubAuthControllerErased for T {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, ()> {
        let toolbox = toolbox.clone();

        async move {
            let buf = serde_json::to_string(&param).unwrap();
            let data: T::Request = match serde_json::from_value(param) {
                Ok(data) => data,
                Err(err) => {
                    let jd = &mut serde_json::Deserializer::from_str(&buf);
                    let data: std::result::Result<T::Request, _> =
                        serde_path_to_error::deserialize(jd);
                    let path = data.err().map(|err| err.path().to_string());
                    toolbox.send(
                        ctx.connection_id,
                        request_error_to_resp(
                            &ctx,
                            ErrorCode::BAD_REQUEST,
                            if let Some(path) = path {
                                format!("{path}: {err}")
                            } else {
                                format!("{err}")
                            },
                        ),
                    );
                    return;
                }
            };

            let resp = SubAuthController::auth(self, &toolbox, data, ctx.clone(), conn).await;
            if let Some(resp) =
                Toolbox::encode_handler_response::<T::Request, T::Error>(ctx.clone(), resp)
            {
                toolbox.send(ctx.connection_id, resp);
            }
        }
        .boxed_local()
    }
}

pub struct EndpointAuthController {
    pub auth_endpoints: HashMap<String, WsAuthController>,
}
pub struct WsAuthController {
    pub schema: EndpointSchema,
    pub handler: Arc<dyn SubAuthControllerErased>,
}

impl Default for EndpointAuthController {
    fn default() -> Self {
        Self::new()
    }
}

impl EndpointAuthController {
    pub fn new() -> Self {
        Self {
            auth_endpoints: Default::default(),
        }
    }
    pub fn add_auth_endpoint(
        &mut self,
        schema: EndpointSchema,
        handler: impl SubAuthController + 'static,
    ) {
        self.auth_endpoints.insert(
            schema.name.to_ascii_lowercase(),
            WsAuthController {
                schema,
                handler: Arc::new(handler),
            },
        );
    }
}
fn parse_ty(ty: &Type, value: &str) -> Result<serde_json::Value> {
    Ok(match &ty {
        Type::String => {
            let decoded = urlencoding::decode(value)?;
            serde_json::Value::String(decoded.to_string())
        }
        Type::Int64 => serde_json::Value::Number(
            value
                .parse::<i64>()
                .with_context(|| format!("Failed to parse integer: {value}"))?
                .into(),
        ),
        Type::Boolean => serde_json::Value::Bool(
            value
                .parse::<bool>()
                .with_context(|| format!("Failed to parse boolean: {value}"))?,
        ),
        Type::Enum { .. } => serde_json::Value::String(value.to_string()),
        Type::EnumRef { .. } => serde_json::Value::String(value.to_string()),
        Type::UUID => serde_json::Value::String(value.to_string()),
        Type::NanoId { .. } => serde_json::Value::String(value.to_string()),
        Type::Optional(ty) => parse_ty(ty, value)?,
        Type::BlockchainAddress => serde_json::Value::String(value.to_string()),
        ty => bail!("Not implemented {:?}", ty),
    })
}

fn parse_protocol_header(header: &str) -> HashMap<&str, &str> {
    header
        .split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .map(|x| (&x[..1], &x[1..]))
        .collect()
}

impl AuthController for EndpointAuthController {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let toolbox = toolbox.clone();

        async move {
            let splits = parse_protocol_header(&header);

            debug!(ws_server = true, raw_header = %header, splits = ?splits, "EndpointAuthController: parsed protocol header");

            let method = splits.get("0").context("Could not find method")?;
            debug!(ws_server = true, method = %method, "EndpointAuthController: resolved method");
            let endpoint = self
                .auth_endpoints
                .get(*method)
                .with_context(|| format!("Could not find endpoint for method {method}"))?;
            let mut params = serde_json::Map::new();
            for (index, param) in endpoint.schema.parameters.iter().enumerate() {
                let index = index + 1;
                match splits.get(&index.to_string().as_str()) {
                    Some(value) => {
                        params.insert(param.name.to_case(Case::Camel), parse_ty(&param.ty, value)?);
                    }
                    None if !matches!(&param.ty, Type::Optional(_)) => {
                        bail!("Could not find param {} {}", param.name, index);
                    }
                    _ => {}
                }
            }
            let roles = conn.roles.read().clone();
            let ctx = RequestContext {
                connection_id: conn.connection_id,
                user_id: 0,
                seq: 0,
                method: endpoint.schema.code,
                log_id: conn.log_id,
                roles: roles.clone(),
                ip_addr: conn.address.ip(),
            };
            endpoint
                .handler
                .clone()
                .auth(
                    &toolbox,
                    serde_json::Value::Object(params),
                    ctx.clone(),
                    conn,
                )
                .await;
            debug!(ws_server = true, "Auth response sent");
            Ok(())
        }
        .boxed_local()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_header() {
        let map = parse_protocol_header("");
        assert!(map.is_empty());
    }

    #[test]
    fn parse_single_method() {
        let map = parse_protocol_header("0my_endpoint");
        assert_eq!(map.get("0"), Some(&"my_endpoint"));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn parse_method_and_token() {
        let map = parse_protocol_header("0login,1abc123");
        assert_eq!(map.get("0"), Some(&"login"));
        assert_eq!(map.get("1"), Some(&"abc123"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_multiple_params() {
        let map = parse_protocol_header("0my_endpoint,1token_value,2user_123,3extra");
        assert_eq!(map.get("0"), Some(&"my_endpoint"));
        assert_eq!(map.get("1"), Some(&"token_value"));
        assert_eq!(map.get("2"), Some(&"user_123"));
        assert_eq!(map.get("3"), Some(&"extra"));
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn parse_whitespace_tolerance() {
        let map = parse_protocol_header("0method, 1value , 2another");
        assert_eq!(map.get("0"), Some(&"method"));
        assert_eq!(map.get("1"), Some(&"value"));
        assert_eq!(map.get("2"), Some(&"another"));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn parse_empty_segments_filtered() {
        let map = parse_protocol_header("0method,,1value");
        assert_eq!(map.get("0"), Some(&"method"));
        assert_eq!(map.get("1"), Some(&"value"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_leading_trailing_commas() {
        let map = parse_protocol_header(",0method,1value,");
        assert_eq!(map.get("0"), Some(&"method"));
        assert_eq!(map.get("1"), Some(&"value"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_comma_in_value() {
        let map = parse_protocol_header("0method,1val,ue");
        assert_eq!(map.get("0"), Some(&"method"));
        assert_eq!(map.get("1"), Some(&"val"));
        assert_eq!(map.get("u"), Some(&"e"));
        assert_eq!(map.len(), 3);
    }
}
