use serde::*;
use std::str::FromStr;

/// `ErrorCode` is a wrapper around `u32` that represents an error code.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ErrorCode {
    /// The error code (e.g. `100400` for BadRequest).
    code: u32,
}

impl ErrorCode {
    /// Indicated a bad request.
    pub const BAD_REQUEST: Self = Self::new(100400);

    /// Indicates that authentication is required.
    pub const UNAUTHORIZED: Self = Self::new(100401);

    /// Indicates that payment is required.
    pub const PAYMENT_REQUIRED: Self = Self::new(100402);

    /// Indicates forbidden access.
    pub const FORBIDDEN: Self = Self::new(100403);

    /// Indicates that the requested resource was not found.
    pub const NOT_FOUND: Self = Self::new(100404);

    /// Indicates that the request method is not allowed.
    pub const METHOD_NOT_ALLOWED: Self = Self::new(100405);

    /// Indicates that the requested response format is not acceptable.
    pub const NOT_ACCEPTABLE: Self = Self::new(100406);

    /// Indicates that proxy authentication is required.
    pub const PROXY_AUTHENTICATION_REQUIRED: Self = Self::new(100407);

    /// Indicates that the request timed out.
    pub const REQUEST_TIMEOUT: Self = Self::new(100408);

    /// Indicates that the request conflicts with current state.
    pub const CONFLICT: Self = Self::new(100409);

    /// Indicates that the requested resource is gone.
    pub const GONE: Self = Self::new(100410);

    /// Indicates that the request must include a content length.
    pub const LENGTH_REQUIRED: Self = Self::new(100411);

    /// Indicates that a precondition failed.
    pub const PRECONDITION_FAILED: Self = Self::new(100412);

    /// Indicates that the payload is too large.
    pub const PAYLOAD_TOO_LARGE: Self = Self::new(100413);

    /// Indicates that the URI is too long.
    pub const URI_TOO_LONG: Self = Self::new(100414);

    /// Indicates that the media type is unsupported.
    pub const UNSUPPORTED_MEDIA_TYPE: Self = Self::new(100415);

    /// Indicates that the requested range cannot be satisfied.
    pub const RANGE_NOT_SATISFIABLE: Self = Self::new(100416);

    /// Indicates that an expectation failed.
    pub const EXPECTATION_FAILED: Self = Self::new(100417);

    /// Indicates an I'm a teapot response.
    pub const IM_A_TEAPOT: Self = Self::new(100418);

    /// Indicates that the request was misdirected.
    pub const MISDIRECTED_REQUEST: Self = Self::new(100421);

    /// Indicates that the entity could not be processed.
    pub const UNPROCESSABLE_ENTITY: Self = Self::new(100422);

    /// Indicates that the resource is locked.
    pub const LOCKED: Self = Self::new(100423);

    /// Indicates a failed dependency.
    pub const FAILED_DEPENDENCY: Self = Self::new(100424);

    /// Indicates that the request must be upgraded.
    pub const UPGRADE_REQUIRED: Self = Self::new(100426);

    /// Indicates that a precondition is required.
    pub const PRECONDITION_REQUIRED: Self = Self::new(100428);

    /// Indicates too many requests.
    pub const TOO_MANY_REQUESTS: Self = Self::new(100429);

    /// Indicates that request header fields are too large.
    pub const REQUEST_HEADER_FIELDS_TOO_LARGE: Self = Self::new(100431);

    /// Indicates that the request is unavailable for legal reasons.
    pub const UNAVAILABLE_FOR_LEGAL_REASONS: Self = Self::new(100451);

    /// Indicates an internal server error.
    pub const INTERNAL_ERROR: Self = Self::new(100500);

    /// Indicates a non-implemented endpoint.
    pub const NOT_IMPLEMENTED: Self = Self::new(100501);

    /// Indicates a bad gateway.
    pub const BAD_GATEWAY: Self = Self::new(100502);

    /// Indicates that the service is unavailable.
    pub const SERVICE_UNAVAILABLE: Self = Self::new(100503);

    /// Indicates a gateway timeout.
    pub const GATEWAY_TIMEOUT: Self = Self::new(100504);

    /// Indicates that the HTTP version is not supported.
    pub const HTTP_VERSION_NOT_SUPPORTED: Self = Self::new(100505);

    /// Indicates that content negotiation also found a variant problem.
    pub const VARIANT_ALSO_NEGOTIATES: Self = Self::new(100506);

    /// Indicates insufficient storage.
    pub const INSUFFICIENT_STORAGE: Self = Self::new(100507);

    /// Indicates that a loop was detected.
    pub const LOOP_DETECTED: Self = Self::new(100508);

    /// Indicates that the request must be extended.
    pub const NOT_EXTENDED: Self = Self::new(100510);

    /// Indicates that network authentication is required.
    pub const NETWORK_AUTHENTICATION_REQUIRED: Self = Self::new(100511);

    /// Create a new `ErrorCode` from a `u32`.
    pub const fn new(code: u32) -> Self {
        Self { code }
    }

    /// Get the `ErrorCode` as a `u32`.
    /// Just returns the code field.
    pub const fn to_u32(self) -> u32 {
        self.code
    }

    /// Getter for the `ErrorCode` code field.
    pub const fn code(self) -> u32 {
        self.to_u32()
    }

    pub const fn kind(self) -> &'static str {
        match self.code {
            100400 => "BadRequest",
            100401 => "Unauthorized",
            100402 => "PaymentRequired",
            100403 => "Forbidden",
            100404 => "NotFound",
            100405 => "MethodNotAllowed",
            100406 => "NotAcceptable",
            100407 => "ProxyAuthenticationRequired",
            100408 => "RequestTimeout",
            100409 => "Conflict",
            100410 => "Gone",
            100411 => "LengthRequired",
            100412 => "PreconditionFailed",
            100413 => "PayloadTooLarge",
            100414 => "UriTooLong",
            100415 => "UnsupportedMediaType",
            100416 => "RangeNotSatisfiable",
            100417 => "ExpectationFailed",
            100418 => "ImATeapot",
            100421 => "MisdirectedRequest",
            100422 => "UnprocessableEntity",
            100423 => "Locked",
            100424 => "FailedDependency",
            100426 => "UpgradeRequired",
            100428 => "PreconditionRequired",
            100429 => "TooManyRequests",
            100431 => "RequestHeaderFieldsTooLarge",
            100451 => "UnavailableForLegalReasons",
            100500 => "InternalError",
            100501 => "NotImplemented",
            100502 => "BadGateway",
            100503 => "ServiceUnavailable",
            100504 => "GatewayTimeout",
            100505 => "HttpVersionNotSupported",
            100506 => "VariantAlsoNegotiates",
            100507 => "InsufficientStorage",
            100508 => "LoopDetected",
            100510 => "NotExtended",
            100511 => "NetworkAuthenticationRequired",
            _ => "CustomError",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        let normalized: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(char::to_uppercase)
            .collect();

        match normalized.as_str() {
            "BADREQUEST" => Some(Self::BAD_REQUEST),
            "UNAUTHORIZED" => Some(Self::UNAUTHORIZED),
            "PAYMENTREQUIRED" => Some(Self::PAYMENT_REQUIRED),
            "FORBIDDEN" => Some(Self::FORBIDDEN),
            "NOTFOUND" => Some(Self::NOT_FOUND),
            "METHODNOTALLOWED" => Some(Self::METHOD_NOT_ALLOWED),
            "NOTACCEPTABLE" => Some(Self::NOT_ACCEPTABLE),
            "PROXYAUTHENTICATIONREQUIRED" => Some(Self::PROXY_AUTHENTICATION_REQUIRED),
            "REQUESTTIMEOUT" => Some(Self::REQUEST_TIMEOUT),
            "CONFLICT" => Some(Self::CONFLICT),
            "GONE" => Some(Self::GONE),
            "LENGTHREQUIRED" => Some(Self::LENGTH_REQUIRED),
            "PRECONDITIONFAILED" => Some(Self::PRECONDITION_FAILED),
            "PAYLOADTOOLARGE" => Some(Self::PAYLOAD_TOO_LARGE),
            "URITOOLONG" => Some(Self::URI_TOO_LONG),
            "UNSUPPORTEDMEDIATYPE" => Some(Self::UNSUPPORTED_MEDIA_TYPE),
            "RANGENOTSATISFIABLE" => Some(Self::RANGE_NOT_SATISFIABLE),
            "EXPECTATIONFAILED" => Some(Self::EXPECTATION_FAILED),
            "IMATEAPOT" => Some(Self::IM_A_TEAPOT),
            "MISDIRECTEDREQUEST" => Some(Self::MISDIRECTED_REQUEST),
            "UNPROCESSABLEENTITY" => Some(Self::UNPROCESSABLE_ENTITY),
            "LOCKED" => Some(Self::LOCKED),
            "FAILEDDEPENDENCY" => Some(Self::FAILED_DEPENDENCY),
            "UPGRADEREQUIRED" => Some(Self::UPGRADE_REQUIRED),
            "PRECONDITIONREQUIRED" => Some(Self::PRECONDITION_REQUIRED),
            "TOOMANYREQUESTS" => Some(Self::TOO_MANY_REQUESTS),
            "REQUESTHEADERFIELDSTOOLARGE" => Some(Self::REQUEST_HEADER_FIELDS_TOO_LARGE),
            "UNAVAILABLEFORLEGALREASONS" => Some(Self::UNAVAILABLE_FOR_LEGAL_REASONS),
            "INTERNALERROR" => Some(Self::INTERNAL_ERROR),
            "NOTIMPLEMENTED" => Some(Self::NOT_IMPLEMENTED),
            "BADGATEWAY" => Some(Self::BAD_GATEWAY),
            "SERVICEUNAVAILABLE" => Some(Self::SERVICE_UNAVAILABLE),
            "GATEWAYTIMEOUT" => Some(Self::GATEWAY_TIMEOUT),
            "HTTPVERSIONNOTSUPPORTED" => Some(Self::HTTP_VERSION_NOT_SUPPORTED),
            "VARIANTALSONEGOTIATES" => Some(Self::VARIANT_ALSO_NEGOTIATES),
            "INSUFFICIENTSTORAGE" => Some(Self::INSUFFICIENT_STORAGE),
            "LOOPDETECTED" => Some(Self::LOOP_DETECTED),
            "NOTEXTENDED" => Some(Self::NOT_EXTENDED),
            "NETWORKAUTHENTICATIONREQUIRED" => Some(Self::NETWORK_AUTHENTICATION_REQUIRED),
            _ => None,
        }
    }
}

impl FromStr for ErrorCode {
    type Err = ParseErrorCodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some(code) = Self::from_name(value) {
            return Ok(code);
        }

        value
            .parse::<u32>()
            .map(Self::new)
            .map_err(|_| ParseErrorCodeError)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseErrorCodeError;

impl std::fmt::Display for ParseErrorCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid error code")
    }
}

impl std::error::Error for ParseErrorCodeError {}
