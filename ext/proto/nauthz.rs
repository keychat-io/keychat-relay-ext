// This file is @generated by prost-build.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Event {
    /// 32-byte SHA256 hash of serialized event
    #[prost(bytes = "vec", tag = "1")]
    pub id: ::prost::alloc::vec::Vec<u8>,
    /// 32-byte public key of event creator
    #[prost(bytes = "vec", tag = "2")]
    pub pubkey: ::prost::alloc::vec::Vec<u8>,
    /// UNIX timestamp provided by event creator
    #[prost(fixed64, tag = "3")]
    pub created_at: u64,
    /// event kind
    #[prost(uint64, tag = "4")]
    pub kind: u64,
    /// arbitrary event contents
    #[prost(string, tag = "5")]
    pub content: ::prost::alloc::string::String,
    /// event tag array
    #[prost(message, repeated, tag = "6")]
    pub tags: ::prost::alloc::vec::Vec<event::TagEntry>,
    /// 32-byte signature of the event id
    #[prost(bytes = "vec", tag = "7")]
    pub sig: ::prost::alloc::vec::Vec<u8>,
    /// optional cashu token
    #[prost(string, optional, tag = "8")]
    pub cashu: ::core::option::Option<::prost::alloc::string::String>,
}
/// Nested message and enum types in `Event`.
pub mod event {
    /// Individual values for a single tag
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct TagEntry {
        #[prost(string, repeated, tag = "1")]
        pub values: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
    }
}
/// Event data and metadata for authorization decisions
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct EventRequest {
    /// the event to be admitted for further relay processing
    #[prost(message, optional, tag = "1")]
    pub event: ::core::option::Option<Event>,
    /// IP address of the client that submitted the event
    #[prost(string, optional, tag = "2")]
    pub ip_addr: ::core::option::Option<::prost::alloc::string::String>,
    /// HTTP origin header from the client, if one exists
    #[prost(string, optional, tag = "3")]
    pub origin: ::core::option::Option<::prost::alloc::string::String>,
    /// HTTP user-agent header from the client, if one exists
    #[prost(string, optional, tag = "4")]
    pub user_agent: ::core::option::Option<::prost::alloc::string::String>,
    /// the public key associated with a NIP-42 AUTH'd session, if
    #[prost(bytes = "vec", optional, tag = "5")]
    pub auth_pubkey: ::core::option::Option<::prost::alloc::vec::Vec<u8>>,
    /// authentication occurred
    ///
    /// NIP-05 address associated with the event pubkey, if it is
    #[prost(message, optional, tag = "6")]
    pub nip05: ::core::option::Option<event_request::Nip05Name>,
}
/// Nested message and enum types in `EventRequest`.
pub mod event_request {
    /// known and has been validated by the relay
    /// A NIP_05 verification record
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Nip05Name {
        #[prost(string, tag = "1")]
        pub local: ::prost::alloc::string::String,
        #[prost(string, tag = "2")]
        pub domain: ::prost::alloc::string::String,
    }
}
/// Response to a event authorization request
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct EventReply {
    /// decision to enforce
    #[prost(enumeration = "Decision", tag = "1")]
    pub decision: i32,
    /// informative message for the client
    #[prost(string, optional, tag = "2")]
    pub message: ::core::option::Option<::prost::alloc::string::String>,
}
/// A permit or deny decision
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum Decision {
    Unspecified = 0,
    /// Admit this event for further processing
    Permit = 1,
    /// Deny persisting or propagating this event
    Deny = 2,
}
impl Decision {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            Decision::Unspecified => "DECISION_UNSPECIFIED",
            Decision::Permit => "DECISION_PERMIT",
            Decision::Deny => "DECISION_DENY",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "DECISION_UNSPECIFIED" => Some(Self::Unspecified),
            "DECISION_PERMIT" => Some(Self::Permit),
            "DECISION_DENY" => Some(Self::Deny),
            _ => None,
        }
    }
}
/// Generated server implementations.
pub mod authorization_server {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with AuthorizationServer.
    #[async_trait]
    pub trait Authorization: Send + Sync + 'static {
        /// Determine if an event should be admitted to the relay
        async fn event_admit(
            &self,
            request: tonic::Request<super::EventRequest>,
        ) -> std::result::Result<tonic::Response<super::EventReply>, tonic::Status>;
    }
    /// Authorization for actions against a relay
    #[derive(Debug)]
    pub struct AuthorizationServer<T: Authorization> {
        inner: _Inner<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    struct _Inner<T>(Arc<T>);
    impl<T: Authorization> AuthorizationServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            let inner = _Inner(inner);
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for AuthorizationServer<T>
    where
        T: Authorization,
        B: Body + Send + 'static,
        B::Error: Into<StdError> + Send + 'static,
    {
        type Response = http::Response<tonic::body::BoxBody>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            let inner = self.inner.clone();
            match req.uri().path() {
                "/nauthz.Authorization/EventAdmit" => {
                    #[allow(non_camel_case_types)]
                    struct EventAdmitSvc<T: Authorization>(pub Arc<T>);
                    impl<
                        T: Authorization,
                    > tonic::server::UnaryService<super::EventRequest>
                    for EventAdmitSvc<T> {
                        type Response = super::EventReply;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::EventRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Authorization>::event_admit(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = EventAdmitSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => {
                    Box::pin(async move {
                        Ok(
                            http::Response::builder()
                                .status(200)
                                .header("grpc-status", "12")
                                .header("content-type", "application/grpc")
                                .body(empty_body())
                                .unwrap(),
                        )
                    })
                }
            }
        }
    }
    impl<T: Authorization> Clone for AuthorizationServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    impl<T: Authorization> Clone for _Inner<T> {
        fn clone(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
    }
    impl<T: std::fmt::Debug> std::fmt::Debug for _Inner<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}", self.0)
        }
    }
    impl<T: Authorization> tonic::server::NamedService for AuthorizationServer<T> {
        const NAME: &'static str = "nauthz.Authorization";
    }
}
