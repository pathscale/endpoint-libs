use convert_case::Case;
use convert_case::Casing;
use derive_more::Display;
use eyre::{Context, Result, bail};
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::*;

use crate::libs::error_code::ErrorCode;
use crate::libs::toolbox::{ArcToolbox, RequestContext, Toolbox};
use crate::model::EndpointSchema;
use crate::model::Type;

use super::WsConnection;
use super::toolbox::{CustomError, HandlerError};

pub trait AuthController: Sync + Send {
    type Error: HandlerError;

    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<(), Self::Error>>;
}

pub struct SimpleAuthController;

impl AuthController for SimpleAuthController {
    type Error = CustomError;

    fn auth(
        self: Arc<Self>,
        _toolbox: &ArcToolbox,
        _header: String,
        _conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<(), CustomError>> {
        async move { Ok(()) }.boxed()
    }
}

pub trait SubAuthController: Sync + Send {
    type Error: HandlerError;

    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<serde_json::Value, Self::Error>>;
}

pub trait SubAuthControllerErased: Sync + Send {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<serde_json::Value, CustomError>>;
}

impl<T: SubAuthController + 'static> SubAuthControllerErased for T {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<serde_json::Value, CustomError>> {
        let toolbox = toolbox.clone();
        async move {
            SubAuthController::auth(self, &toolbox, param, ctx, conn)
                .await
                .map_err(|e| e.into())
        }
        .boxed_local()
    }
}

pub trait AuthControllerErased: Sync + Send {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<(), CustomError>>;
}

impl<T: AuthController + 'static> AuthControllerErased for T {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<(), CustomError>> {
        let toolbox = toolbox.clone();
        async move {
            AuthController::auth(self, &toolbox, header, conn)
                .await
                .map_err(|e| e.into())
        }
        .boxed_local()
    }
}

#[derive(Debug, Clone, Display)]
pub enum EndpointAuthError {
    #[display("Could not find method")]
    MissingMethod,
    #[display("Could not find endpoint for method {method}")]
    EndpointNotFound { method: String },
    #[display("Failed to parse parameter {param}: {reason}")]
    ParameterParseError { param: String, reason: String },
    #[display("Could not find param {param} {index}")]
    MissingRequiredParameter { param: String, index: usize },
}

impl std::error::Error for EndpointAuthError {}

impl HandlerError for EndpointAuthError {
    fn error_code(&self) -> ErrorCode {
        ErrorCode::BAD_REQUEST
    }

    fn params(&self) -> String {
        self.to_string()
    }
}

impl From<EndpointAuthError> for CustomError {
    fn from(err: EndpointAuthError) -> Self {
        CustomError::new(err.error_code(), err.params())
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
    type Error = EndpointAuthError;

    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<(), EndpointAuthError>> {
        let toolbox = toolbox.clone();

        async move {
            let splits = parse_protocol_header(&header);

            debug!(ws_server = true, raw_header = %header, splits = ?splits, "EndpointAuthController: parsed protocol header");

            let method = splits.get("0").ok_or(EndpointAuthError::MissingMethod)?;
            debug!(ws_server = true, method = %method, "EndpointAuthController: resolved method");
            let endpoint = self
                .auth_endpoints
                .get(*method)
                .ok_or_else(|| EndpointAuthError::EndpointNotFound { method: method.to_string() })?;
            let mut params = serde_json::Map::new();
            for (index, param) in endpoint.schema.parameters.iter().enumerate() {
                let index = index + 1;
                match splits.get(&index.to_string().as_str()) {
                    Some(value) => {
                        params.insert(
                            param.name.to_case(Case::Camel),
                            parse_ty(&param.ty, value).map_err(|e| EndpointAuthError::ParameterParseError {
                                param: param.name.clone(),
                                reason: e.to_string(),
                            })?,
                        );
                    }
                    None if !matches!(&param.ty, Type::Optional(_)) => {
                        return Err(EndpointAuthError::MissingRequiredParameter {
                            param: param.name.clone(),
                            index,
                        });
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
            let resp = endpoint
                .handler
                .clone()
                .auth(
                    &toolbox,
                    serde_json::Value::Object(params),
                    ctx.clone(),
                    conn,
                )
                .await;
            debug!(ws_server = true, "Auth response: {:?}", resp);
            let conn_id = ctx.connection_id;
            if let Some(resp) = Toolbox::encode_ws_response(ctx, resp) {
                toolbox.send(conn_id, resp);
            }
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
