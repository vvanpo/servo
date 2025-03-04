/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::document_loader::DocumentLoader;
use crate::dom::bindings::cell::DomRefCell;
use crate::dom::bindings::codegen::Bindings::BlobBinding::BlobBinding::BlobMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::codegen::Bindings::XMLHttpRequestBinding;
use crate::dom::bindings::codegen::Bindings::XMLHttpRequestBinding::BodyInit;
use crate::dom::bindings::codegen::Bindings::XMLHttpRequestBinding::XMLHttpRequestMethods;
use crate::dom::bindings::codegen::Bindings::XMLHttpRequestBinding::XMLHttpRequestResponseType;
use crate::dom::bindings::codegen::UnionTypes::DocumentOrBodyInit;
use crate::dom::bindings::conversions::ToJSValConvertible;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::{reflect_dom_object, DomObject};
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::{is_token, ByteString, DOMString, USVString};
use crate::dom::blob::{Blob, BlobImpl};
use crate::dom::document::DocumentSource;
use crate::dom::document::{Document, HasBrowsingContext, IsHTMLDocument};
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::formdata::FormData;
use crate::dom::globalscope::GlobalScope;
use crate::dom::headers::is_forbidden_header_name;
use crate::dom::htmlformelement::{encode_multipart_form_data, generate_boundary};
use crate::dom::node::Node;
use crate::dom::performanceresourcetiming::InitiatorType;
use crate::dom::progressevent::ProgressEvent;
use crate::dom::servoparser::ServoParser;
use crate::dom::urlsearchparams::URLSearchParams;
use crate::dom::window::Window;
use crate::dom::workerglobalscope::WorkerGlobalScope;
use crate::dom::xmlhttprequesteventtarget::XMLHttpRequestEventTarget;
use crate::dom::xmlhttprequestupload::XMLHttpRequestUpload;
use crate::fetch::FetchCanceller;
use crate::network_listener::{self, NetworkListener, PreInvoke, ResourceTimingListener};
use crate::script_runtime::JSContext;
use crate::task_source::networking::NetworkingTaskSource;
use crate::task_source::TaskSourceName;
use crate::timers::{OneshotTimerCallback, OneshotTimerHandle};
use dom_struct::dom_struct;
use encoding_rs::{Encoding, UTF_8};
use euclid::Length;
use headers::{ContentLength, ContentType, HeaderMapExt};
use html5ever::serialize;
use html5ever::serialize::SerializeOpts;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::Method;
use hyper_serde::Serde;
use ipc_channel::ipc;
use ipc_channel::router::ROUTER;
use js::jsapi::JS_ClearPendingException;
use js::jsapi::{Heap, JSObject};
use js::jsval::{JSVal, NullValue, UndefinedValue};
use js::rust::wrappers::JS_ParseJSON;
use js::typedarray::{ArrayBuffer, CreateWith};
use mime::{self, Mime, Name};
use net_traits::request::{CredentialsMode, Destination, Referrer, RequestBuilder, RequestMode};
use net_traits::trim_http_whitespace;
use net_traits::CoreResourceMsg::Fetch;
use net_traits::{FetchChannels, FetchMetadata, FilteredMetadata};
use net_traits::{FetchResponseListener, NetworkError, ReferrerPolicy};
use net_traits::{ResourceFetchTiming, ResourceTimingType};
use script_traits::DocumentActivity;
use servo_atoms::Atom;
use servo_url::ServoUrl;
use std::borrow::ToOwned;
use std::cell::Cell;
use std::cmp;
use std::default::Default;
use std::ptr;
use std::ptr::NonNull;
use std::slice;
use std::str::{self, FromStr};
use std::sync::{Arc, Mutex};
use url::Position;

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
enum XMLHttpRequestState {
    Unsent = 0,
    Opened = 1,
    HeadersReceived = 2,
    Loading = 3,
    Done = 4,
}

#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
pub struct GenerationId(u32);

/// Closure of required data for each async network event that comprises the
/// XHR's response.
struct XHRContext {
    xhr: TrustedXHRAddress,
    gen_id: GenerationId,
    sync_status: DomRefCell<Option<ErrorResult>>,
    resource_timing: ResourceFetchTiming,
    url: ServoUrl,
}

#[derive(Clone)]
pub enum XHRProgress {
    /// Notify that headers have been received
    HeadersReceived(GenerationId, Option<HeaderMap>, Option<(u16, Vec<u8>)>),
    /// Partial progress (after receiving headers), containing portion of the response
    Loading(GenerationId, Vec<u8>),
    /// Loading is done
    Done(GenerationId),
    /// There was an error (only Error::Abort, Error::Timeout or Error::Network is used)
    Errored(GenerationId, Error),
}

impl XHRProgress {
    fn generation_id(&self) -> GenerationId {
        match *self {
            XHRProgress::HeadersReceived(id, _, _) |
            XHRProgress::Loading(id, _) |
            XHRProgress::Done(id) |
            XHRProgress::Errored(id, _) => id,
        }
    }
}

#[dom_struct]
pub struct XMLHttpRequest {
    eventtarget: XMLHttpRequestEventTarget,
    ready_state: Cell<XMLHttpRequestState>,
    timeout: Cell<u32>,
    with_credentials: Cell<bool>,
    upload: Dom<XMLHttpRequestUpload>,
    response_url: DomRefCell<String>,
    status: Cell<u16>,
    status_text: DomRefCell<ByteString>,
    response: DomRefCell<Vec<u8>>,
    response_type: Cell<XMLHttpRequestResponseType>,
    response_xml: MutNullableDom<Document>,
    response_blob: MutNullableDom<Blob>,
    #[ignore_malloc_size_of = "mozjs"]
    response_arraybuffer: Heap<*mut JSObject>,
    #[ignore_malloc_size_of = "Defined in rust-mozjs"]
    response_json: Heap<JSVal>,
    #[ignore_malloc_size_of = "Defined in hyper"]
    response_headers: DomRefCell<HeaderMap>,
    #[ignore_malloc_size_of = "Defined in hyper"]
    override_mime_type: DomRefCell<Option<Mime>>,
    override_charset: DomRefCell<Option<&'static Encoding>>,

    // Associated concepts
    #[ignore_malloc_size_of = "Defined in hyper"]
    request_method: DomRefCell<Method>,
    request_url: DomRefCell<Option<ServoUrl>>,
    #[ignore_malloc_size_of = "Defined in hyper"]
    request_headers: DomRefCell<HeaderMap>,
    request_body_len: Cell<usize>,
    sync: Cell<bool>,
    upload_complete: Cell<bool>,
    send_flag: Cell<bool>,

    timeout_cancel: DomRefCell<Option<OneshotTimerHandle>>,
    fetch_time: Cell<i64>,
    generation_id: Cell<GenerationId>,
    response_status: Cell<Result<(), ()>>,
    referrer_url: Option<ServoUrl>,
    referrer_policy: Option<ReferrerPolicy>,
    canceller: DomRefCell<FetchCanceller>,
}

impl XMLHttpRequest {
    fn new_inherited(global: &GlobalScope) -> XMLHttpRequest {
        //TODO - update this when referrer policy implemented for workers
        let (referrer_url, referrer_policy) = if let Some(window) = global.downcast::<Window>() {
            let document = window.Document();
            (Some(document.url()), document.get_referrer_policy())
        } else {
            (None, None)
        };

        XMLHttpRequest {
            eventtarget: XMLHttpRequestEventTarget::new_inherited(),
            ready_state: Cell::new(XMLHttpRequestState::Unsent),
            timeout: Cell::new(0u32),
            with_credentials: Cell::new(false),
            upload: Dom::from_ref(&*XMLHttpRequestUpload::new(global)),
            response_url: DomRefCell::new(String::new()),
            status: Cell::new(0),
            status_text: DomRefCell::new(ByteString::new(vec![])),
            response: DomRefCell::new(vec![]),
            response_type: Cell::new(XMLHttpRequestResponseType::_empty),
            response_xml: Default::default(),
            response_blob: Default::default(),
            response_arraybuffer: Heap::default(),
            response_json: Heap::default(),
            response_headers: DomRefCell::new(HeaderMap::new()),
            override_mime_type: DomRefCell::new(None),
            override_charset: DomRefCell::new(None),

            request_method: DomRefCell::new(Method::GET),
            request_url: DomRefCell::new(None),
            request_headers: DomRefCell::new(HeaderMap::new()),
            request_body_len: Cell::new(0),
            sync: Cell::new(false),
            upload_complete: Cell::new(false),
            send_flag: Cell::new(false),

            timeout_cancel: DomRefCell::new(None),
            fetch_time: Cell::new(0),
            generation_id: Cell::new(GenerationId(0)),
            response_status: Cell::new(Ok(())),
            referrer_url: referrer_url,
            referrer_policy: referrer_policy,
            canceller: DomRefCell::new(Default::default()),
        }
    }
    pub fn new(global: &GlobalScope) -> DomRoot<XMLHttpRequest> {
        reflect_dom_object(
            Box::new(XMLHttpRequest::new_inherited(global)),
            global,
            XMLHttpRequestBinding::Wrap,
        )
    }

    // https://xhr.spec.whatwg.org/#constructors
    pub fn Constructor(global: &GlobalScope) -> Fallible<DomRoot<XMLHttpRequest>> {
        Ok(XMLHttpRequest::new(global))
    }

    fn sync_in_window(&self) -> bool {
        self.sync.get() && self.global().is::<Window>()
    }

    fn initiate_async_xhr(
        context: Arc<Mutex<XHRContext>>,
        task_source: NetworkingTaskSource,
        global: &GlobalScope,
        init: RequestBuilder,
        cancellation_chan: ipc::IpcReceiver<()>,
    ) {
        impl FetchResponseListener for XHRContext {
            fn process_request_body(&mut self) {
                // todo
            }

            fn process_request_eof(&mut self) {
                // todo
            }

            fn process_response(&mut self, metadata: Result<FetchMetadata, NetworkError>) {
                let xhr = self.xhr.root();
                let rv = xhr.process_headers_available(self.gen_id, metadata);
                if rv.is_err() {
                    *self.sync_status.borrow_mut() = Some(rv);
                }
            }

            fn process_response_chunk(&mut self, chunk: Vec<u8>) {
                self.xhr.root().process_data_available(self.gen_id, chunk);
            }

            fn process_response_eof(
                &mut self,
                response: Result<ResourceFetchTiming, NetworkError>,
            ) {
                let rv = self
                    .xhr
                    .root()
                    .process_response_complete(self.gen_id, response.map(|_| ()));
                *self.sync_status.borrow_mut() = Some(rv);
            }

            fn resource_timing_mut(&mut self) -> &mut ResourceFetchTiming {
                &mut self.resource_timing
            }

            fn resource_timing(&self) -> &ResourceFetchTiming {
                &self.resource_timing
            }

            fn submit_resource_timing(&mut self) {
                network_listener::submit_timing(self)
            }
        }

        impl ResourceTimingListener for XHRContext {
            fn resource_timing_information(&self) -> (InitiatorType, ServoUrl) {
                (InitiatorType::XMLHttpRequest, self.url.clone())
            }

            fn resource_timing_global(&self) -> DomRoot<GlobalScope> {
                self.xhr.root().global()
            }
        }

        impl PreInvoke for XHRContext {
            fn should_invoke(&self) -> bool {
                self.xhr.root().generation_id.get() == self.gen_id
            }
        }

        let (action_sender, action_receiver) = ipc::channel().unwrap();

        let listener = NetworkListener {
            context: context,
            task_source: task_source,
            canceller: Some(global.task_canceller(TaskSourceName::Networking)),
        };
        ROUTER.add_route(
            action_receiver.to_opaque(),
            Box::new(move |message| {
                listener.notify_fetch(message.to().unwrap());
            }),
        );
        global
            .core_resource_thread()
            .send(Fetch(
                init,
                FetchChannels::ResponseMsg(action_sender, Some(cancellation_chan)),
            ))
            .unwrap();
    }
}

impl XMLHttpRequestMethods for XMLHttpRequest {
    // https://xhr.spec.whatwg.org/#handler-xhr-onreadystatechange
    event_handler!(
        readystatechange,
        GetOnreadystatechange,
        SetOnreadystatechange
    );

    // https://xhr.spec.whatwg.org/#dom-xmlhttprequest-readystate
    fn ReadyState(&self) -> u16 {
        self.ready_state.get() as u16
    }

    // https://xhr.spec.whatwg.org/#the-open()-method
    fn Open(&self, method: ByteString, url: USVString) -> ErrorResult {
        // Step 8
        self.Open_(method, url, true, None, None)
    }

    // https://xhr.spec.whatwg.org/#the-open()-method
    fn Open_(
        &self,
        method: ByteString,
        url: USVString,
        r#async: bool,
        username: Option<USVString>,
        password: Option<USVString>,
    ) -> ErrorResult {
        // Step 1
        if let Some(window) = DomRoot::downcast::<Window>(self.global()) {
            if !window.Document().is_fully_active() {
                return Err(Error::InvalidState);
            }
        }

        // Step 5
        //FIXME(seanmonstar): use a Trie instead?
        let maybe_method = method.as_str().and_then(|s| {
            // Note: hyper tests against the uppercase versions
            // Since we want to pass methods not belonging to the short list above
            // without changing capitalization, this will actually sidestep rust-http's type system
            // since methods like "patch" or "PaTcH" will be considered extension methods
            // despite the there being a rust-http method variant for them
            let upper = s.to_ascii_uppercase();
            match &*upper {
                "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT" | "CONNECT" | "TRACE" |
                "TRACK" => upper.parse().ok(),
                _ => s.parse().ok(),
            }
        });

        match maybe_method {
            // Step 4
            Some(Method::CONNECT) | Some(Method::TRACE) => Err(Error::Security),
            Some(ref t) if t.as_str() == "TRACK" => Err(Error::Security),
            Some(parsed_method) => {
                // Step 3
                if !is_token(&method) {
                    return Err(Error::Syntax);
                }

                // Step 2
                let base = self.global().api_base_url();
                // Step 6
                let mut parsed_url = match base.join(&url.0) {
                    Ok(parsed) => parsed,
                    // Step 7
                    Err(_) => return Err(Error::Syntax),
                };

                // Step 9
                if parsed_url.host().is_some() {
                    if let Some(user_str) = username {
                        parsed_url.set_username(&user_str.0).unwrap();
                        let password = password.as_ref().map(|pass_str| &*pass_str.0);
                        parsed_url.set_password(password).unwrap();
                    }
                }

                // Step 10
                if !r#async {
                    // FIXME: This should only happen if the global environment is a document environment
                    if self.timeout.get() != 0 ||
                        self.response_type.get() != XMLHttpRequestResponseType::_empty
                    {
                        return Err(Error::InvalidAccess);
                    }
                }
                // Step 11 - abort existing requests
                self.terminate_ongoing_fetch();

                // FIXME(#13767): In the WPT test: FileAPI/blob/Blob-XHR-revoke.html,
                // the xhr.open(url) is expected to hold a reference to the URL,
                // thus renders following revocations invalid. Though we won't
                // implement this for now, if ever needed, we should check blob
                // scheme and trigger corresponding actions here.

                // Step 12
                *self.request_method.borrow_mut() = parsed_method;
                *self.request_url.borrow_mut() = Some(parsed_url);
                self.sync.set(!r#async);
                *self.request_headers.borrow_mut() = HeaderMap::new();
                self.send_flag.set(false);
                *self.status_text.borrow_mut() = ByteString::new(vec![]);
                self.status.set(0);

                // Step 13
                if self.ready_state.get() != XMLHttpRequestState::Opened {
                    self.change_ready_state(XMLHttpRequestState::Opened);
                }
                Ok(())
            },
            // Step 3
            // This includes cases where as_str() returns None, and when is_token() returns false,
            // both of which indicate invalid extension method names
            _ => Err(Error::Syntax),
        }
    }

    // https://xhr.spec.whatwg.org/#the-setrequestheader()-method
    fn SetRequestHeader(&self, name: ByteString, value: ByteString) -> ErrorResult {
        // Step 1, 2
        if self.ready_state.get() != XMLHttpRequestState::Opened || self.send_flag.get() {
            return Err(Error::InvalidState);
        }

        // Step 3
        let value = trim_http_whitespace(&value);

        // Step 4
        if !is_token(&name) || !is_field_value(&value) {
            return Err(Error::Syntax);
        }
        let name_lower = name.to_lower();
        let name_str = match name_lower.as_str() {
            Some(s) => {
                // Step 5
                // Disallowed headers and header prefixes:
                // https://fetch.spec.whatwg.org/#forbidden-header-name
                if is_forbidden_header_name(s) {
                    return Ok(());
                } else {
                    s
                }
            },
            None => unreachable!(),
        };

        debug!(
            "SetRequestHeader: name={:?}, value={:?}",
            name.as_str(),
            str::from_utf8(value).ok()
        );
        let mut headers = self.request_headers.borrow_mut();

        // Step 6
        let value = match headers.get(name_str).map(HeaderValue::as_bytes) {
            Some(raw) => {
                let mut buf = raw.to_vec();
                buf.extend_from_slice(b", ");
                buf.extend_from_slice(value);
                buf
            },
            None => value.into(),
        };

        headers.insert(
            HeaderName::from_str(name_str).unwrap(),
            HeaderValue::from_bytes(&value).unwrap(),
        );
        Ok(())
    }

    // https://xhr.spec.whatwg.org/#the-timeout-attribute
    fn Timeout(&self) -> u32 {
        self.timeout.get()
    }

    // https://xhr.spec.whatwg.org/#the-timeout-attribute
    fn SetTimeout(&self, timeout: u32) -> ErrorResult {
        // Step 1
        if self.sync_in_window() {
            return Err(Error::InvalidAccess);
        }
        // Step 2
        self.timeout.set(timeout);

        if self.send_flag.get() {
            if timeout == 0 {
                self.cancel_timeout();
                return Ok(());
            }
            let progress = time::now().to_timespec().sec - self.fetch_time.get();
            if timeout > (progress * 1000) as u32 {
                self.set_timeout(timeout - (progress * 1000) as u32);
            } else {
                // Immediately execute the timeout steps
                self.set_timeout(0);
            }
        }
        Ok(())
    }

    // https://xhr.spec.whatwg.org/#the-withcredentials-attribute
    fn WithCredentials(&self) -> bool {
        self.with_credentials.get()
    }

    // https://xhr.spec.whatwg.org/#dom-xmlhttprequest-withcredentials
    fn SetWithCredentials(&self, with_credentials: bool) -> ErrorResult {
        match self.ready_state.get() {
            // Step 1
            XMLHttpRequestState::HeadersReceived |
            XMLHttpRequestState::Loading |
            XMLHttpRequestState::Done => Err(Error::InvalidState),
            // Step 2
            _ if self.send_flag.get() => Err(Error::InvalidState),
            // Step 3
            _ => {
                self.with_credentials.set(with_credentials);
                Ok(())
            },
        }
    }

    // https://xhr.spec.whatwg.org/#the-upload-attribute
    fn Upload(&self) -> DomRoot<XMLHttpRequestUpload> {
        DomRoot::from_ref(&*self.upload)
    }

    // https://xhr.spec.whatwg.org/#the-send()-method
    fn Send(&self, data: Option<DocumentOrBodyInit>) -> ErrorResult {
        // Step 1, 2
        if self.ready_state.get() != XMLHttpRequestState::Opened || self.send_flag.get() {
            return Err(Error::InvalidState);
        }

        // Step 3
        let data = match *self.request_method.borrow() {
            Method::GET | Method::HEAD => None,
            _ => data,
        };
        // Step 4 (first half)
        let extracted_or_serialized = match data {
            Some(DocumentOrBodyInit::Document(ref doc)) => {
                let data = Vec::from(serialize_document(&doc)?.as_ref());
                let content_type = if doc.is_html_document() {
                    "text/html;charset=UTF-8"
                } else {
                    "application/xml;charset=UTF-8"
                };
                Some((data, Some(DOMString::from(content_type))))
            },
            Some(DocumentOrBodyInit::Blob(ref b)) => Some(b.extract()),
            Some(DocumentOrBodyInit::FormData(ref formdata)) => Some(formdata.extract()),
            Some(DocumentOrBodyInit::String(ref str)) => Some(str.extract()),
            Some(DocumentOrBodyInit::URLSearchParams(ref urlsp)) => Some(urlsp.extract()),
            Some(DocumentOrBodyInit::ArrayBuffer(ref typedarray)) => {
                Some((typedarray.to_vec(), None))
            },
            Some(DocumentOrBodyInit::ArrayBufferView(ref typedarray)) => {
                Some((typedarray.to_vec(), None))
            },
            None => None,
        };

        self.request_body_len
            .set(extracted_or_serialized.as_ref().map_or(0, |e| e.0.len()));

        // todo preserved headers?

        // Step 6
        self.upload_complete.set(false);
        // Step 7
        self.upload_complete.set(match extracted_or_serialized {
            None => true,
            Some(ref e) if e.0.is_empty() => true,
            _ => false,
        });
        // Step 8
        self.send_flag.set(true);

        // Step 9
        if !self.sync.get() {
            // If one of the event handlers below aborts the fetch by calling
            // abort or open we will need the current generation id to detect it.
            // Substep 1
            let gen_id = self.generation_id.get();
            self.dispatch_response_progress_event(atom!("loadstart"));
            if self.generation_id.get() != gen_id {
                return Ok(());
            }
            // Substep 2
            if !self.upload_complete.get() {
                self.dispatch_upload_progress_event(atom!("loadstart"), Some(0));
                if self.generation_id.get() != gen_id {
                    return Ok(());
                }
            }
        }

        // Step 5
        //TODO - set referrer_policy/referrer_url in request
        let has_handlers = self.upload.upcast::<EventTarget>().has_handlers();
        let credentials_mode = if self.with_credentials.get() {
            CredentialsMode::Include
        } else {
            CredentialsMode::CredentialsSameOrigin
        };
        let use_url_credentials = if let Some(ref url) = *self.request_url.borrow() {
            !url.username().is_empty() || url.password().is_some()
        } else {
            unreachable!()
        };

        let mut request = RequestBuilder::new(self.request_url.borrow().clone().unwrap())
            .method(self.request_method.borrow().clone())
            .headers((*self.request_headers.borrow()).clone())
            .unsafe_request(true)
            // XXXManishearth figure out how to avoid this clone
            .body(extracted_or_serialized.as_ref().map(|e| e.0.clone()))
            // XXXManishearth actually "subresource", but it doesn't exist
            // https://github.com/whatwg/xhr/issues/71
            .destination(Destination::None)
            .synchronous(self.sync.get())
            .mode(RequestMode::CorsMode)
            .use_cors_preflight(has_handlers)
            .credentials_mode(credentials_mode)
            .use_url_credentials(use_url_credentials)
            .origin(self.global().origin().immutable().clone())
            .referrer(
                self.referrer_url
                    .clone()
                    .map(|referrer_url| Referrer::ReferrerUrl(referrer_url)),
            )
            .referrer_policy(self.referrer_policy.clone())
            .pipeline_id(Some(self.global().pipeline_id()));

        // step 4 (second half)
        match extracted_or_serialized {
            Some((_, ref content_type)) => {
                let encoding = match data {
                    Some(DocumentOrBodyInit::String(_)) | Some(DocumentOrBodyInit::Document(_)) =>
                    // XHR spec differs from http, and says UTF-8 should be in capitals,
                    // instead of "utf-8", which is what Hyper defaults to. So not
                    // using content types provided by Hyper.
                    {
                        Some("UTF-8")
                    },
                    _ => None,
                };

                let mut content_type_set = false;
                if let Some(ref ct) = *content_type {
                    if !request.headers.contains_key(header::CONTENT_TYPE) {
                        request
                            .headers
                            .insert(header::CONTENT_TYPE, HeaderValue::from_str(ct).unwrap());
                        content_type_set = true;
                    }
                }

                if !content_type_set {
                    let ct = request.headers.typed_get::<ContentType>();
                    if let Some(ct) = ct {
                        if let Some(encoding) = encoding {
                            let mime: Mime = ct.into();
                            for param in mime.params() {
                                if param.0 == mime::CHARSET {
                                    if !param.1.as_ref().eq_ignore_ascii_case(encoding) {
                                        let new_params: Vec<(Name, Name)> = mime
                                            .params()
                                            .filter(|p| p.0 != mime::CHARSET)
                                            .map(|p| (p.0, p.1))
                                            .collect();

                                        let new_mime = format!(
                                            "{}/{}; charset={}{}{}",
                                            mime.type_().as_ref(),
                                            mime.subtype().as_ref(),
                                            encoding,
                                            if new_params.is_empty() { "" } else { "; " },
                                            new_params
                                                .iter()
                                                .map(|p| format!("{}={}", p.0, p.1))
                                                .collect::<Vec<String>>()
                                                .join("; ")
                                        );
                                        let new_mime: Mime = new_mime.parse().unwrap();
                                        request.headers.typed_insert(ContentType::from(new_mime))
                                    }
                                }
                            }
                        }
                    }
                }
            },
            _ => (),
        }

        self.fetch_time.set(time::now().to_timespec().sec);

        let rv = self.fetch(request, &self.global());
        // Step 10
        if self.sync.get() {
            return rv;
        }

        let timeout = self.timeout.get();
        if timeout > 0 {
            self.set_timeout(timeout);
        }
        Ok(())
    }

    // https://xhr.spec.whatwg.org/#the-abort()-method
    fn Abort(&self) {
        // Step 1
        self.terminate_ongoing_fetch();
        // Step 2
        let state = self.ready_state.get();
        if (state == XMLHttpRequestState::Opened && self.send_flag.get()) ||
            state == XMLHttpRequestState::HeadersReceived ||
            state == XMLHttpRequestState::Loading
        {
            let gen_id = self.generation_id.get();
            self.process_partial_response(XHRProgress::Errored(gen_id, Error::Abort));
            // If open was called in one of the handlers invoked by the
            // above call then we should terminate the abort sequence
            if self.generation_id.get() != gen_id {
                return;
            }
        }
        // Step 3
        self.ready_state.set(XMLHttpRequestState::Unsent);
    }

    // https://xhr.spec.whatwg.org/#the-responseurl-attribute
    fn ResponseURL(&self) -> USVString {
        USVString(self.response_url.borrow().clone())
    }

    // https://xhr.spec.whatwg.org/#the-status-attribute
    fn Status(&self) -> u16 {
        self.status.get()
    }

    // https://xhr.spec.whatwg.org/#the-statustext-attribute
    fn StatusText(&self) -> ByteString {
        self.status_text.borrow().clone()
    }

    // https://xhr.spec.whatwg.org/#the-getresponseheader()-method
    fn GetResponseHeader(&self, name: ByteString) -> Option<ByteString> {
        let headers = self.filter_response_headers();
        let headers = headers.get_all(HeaderName::from_str(&name.as_str()?.to_lowercase()).ok()?);
        let mut first = true;
        let s = headers.iter().fold(Vec::new(), |mut vec, value| {
            if !first {
                vec.extend(", ".as_bytes());
            }
            if let Ok(v) = str::from_utf8(value.as_bytes()).map(|s| s.trim().as_bytes()) {
                vec.extend(v);
                first = false;
            }
            vec
        });

        // There was no header with that name so we never got to change that value
        if first {
            None
        } else {
            Some(ByteString::new(s))
        }
    }

    // https://xhr.spec.whatwg.org/#the-getallresponseheaders()-method
    fn GetAllResponseHeaders(&self) -> ByteString {
        let headers = self.filter_response_headers();
        let keys = headers.keys();
        let v = keys.fold(Vec::new(), |mut vec, k| {
            let values = headers.get_all(k);
            vec.extend(k.as_str().as_bytes());
            vec.extend(": ".as_bytes());
            let mut first = true;
            for value in values {
                if !first {
                    vec.extend(", ".as_bytes());
                    first = false;
                }
                vec.extend(value.as_bytes());
            }
            vec.extend("\r\n".as_bytes());
            vec
        });

        ByteString::new(v)
    }

    // https://xhr.spec.whatwg.org/#the-overridemimetype()-method
    fn OverrideMimeType(&self, mime: DOMString) -> ErrorResult {
        // Step 1
        match self.ready_state.get() {
            XMLHttpRequestState::Loading | XMLHttpRequestState::Done => {
                return Err(Error::InvalidState);
            },
            _ => {},
        }
        // Step 2
        let override_mime = mime.parse::<Mime>().map_err(|_| Error::Syntax)?;
        // Step 3
        let mime_str = override_mime.as_ref();
        let mime_parts: Vec<&str> = mime_str.split(";").collect();
        let mime_no_params = if mime_parts.len() > 1 {
            mime_parts[0].parse().unwrap()
        } else {
            override_mime.clone()
        };

        *self.override_mime_type.borrow_mut() = Some(mime_no_params);
        // Step 4
        let value = override_mime.get_param(mime::CHARSET);
        *self.override_charset.borrow_mut() =
            value.and_then(|value| Encoding::for_label(value.as_ref().as_bytes()));
        Ok(())
    }

    // https://xhr.spec.whatwg.org/#the-responsetype-attribute
    fn ResponseType(&self) -> XMLHttpRequestResponseType {
        self.response_type.get()
    }

    // https://xhr.spec.whatwg.org/#the-responsetype-attribute
    fn SetResponseType(&self, response_type: XMLHttpRequestResponseType) -> ErrorResult {
        // Step 1
        if self.global().is::<WorkerGlobalScope>() &&
            response_type == XMLHttpRequestResponseType::Document
        {
            return Ok(());
        }
        match self.ready_state.get() {
            // Step 2
            XMLHttpRequestState::Loading | XMLHttpRequestState::Done => Err(Error::InvalidState),
            _ => {
                if self.sync_in_window() {
                    // Step 3
                    Err(Error::InvalidAccess)
                } else {
                    // Step 4
                    self.response_type.set(response_type);
                    Ok(())
                }
            },
        }
    }

    #[allow(unsafe_code)]
    // https://xhr.spec.whatwg.org/#the-response-attribute
    fn Response(&self, cx: JSContext) -> JSVal {
        rooted!(in(*cx) let mut rval = UndefinedValue());
        match self.response_type.get() {
            XMLHttpRequestResponseType::_empty | XMLHttpRequestResponseType::Text => unsafe {
                let ready_state = self.ready_state.get();
                // Step 2
                if ready_state == XMLHttpRequestState::Done ||
                    ready_state == XMLHttpRequestState::Loading
                {
                    self.text_response().to_jsval(*cx, rval.handle_mut());
                } else {
                    // Step 1
                    "".to_jsval(*cx, rval.handle_mut());
                }
            },
            // Step 1
            _ if self.ready_state.get() != XMLHttpRequestState::Done => {
                return NullValue();
            },
            // Step 2
            XMLHttpRequestResponseType::Document => unsafe {
                self.document_response().to_jsval(*cx, rval.handle_mut());
            },
            XMLHttpRequestResponseType::Json => unsafe {
                self.json_response(cx).to_jsval(*cx, rval.handle_mut());
            },
            XMLHttpRequestResponseType::Blob => unsafe {
                self.blob_response().to_jsval(*cx, rval.handle_mut());
            },
            XMLHttpRequestResponseType::Arraybuffer => match self.arraybuffer_response(cx) {
                Some(js_object) => unsafe { js_object.to_jsval(*cx, rval.handle_mut()) },
                None => return NullValue(),
            },
        }
        rval.get()
    }

    // https://xhr.spec.whatwg.org/#the-responsetext-attribute
    fn GetResponseText(&self) -> Fallible<USVString> {
        match self.response_type.get() {
            XMLHttpRequestResponseType::_empty | XMLHttpRequestResponseType::Text => {
                Ok(USVString(String::from(match self.ready_state.get() {
                    // Step 3
                    XMLHttpRequestState::Loading | XMLHttpRequestState::Done => {
                        self.text_response()
                    },
                    // Step 2
                    _ => "".to_owned(),
                })))
            },
            // Step 1
            _ => Err(Error::InvalidState),
        }
    }

    // https://xhr.spec.whatwg.org/#the-responsexml-attribute
    fn GetResponseXML(&self) -> Fallible<Option<DomRoot<Document>>> {
        match self.response_type.get() {
            XMLHttpRequestResponseType::_empty | XMLHttpRequestResponseType::Document => {
                // Step 3
                if let XMLHttpRequestState::Done = self.ready_state.get() {
                    Ok(self.document_response())
                } else {
                    // Step 2
                    Ok(None)
                }
            },
            // Step 1
            _ => Err(Error::InvalidState),
        }
    }
}

pub type TrustedXHRAddress = Trusted<XMLHttpRequest>;

impl XMLHttpRequest {
    fn change_ready_state(&self, rs: XMLHttpRequestState) {
        assert_ne!(self.ready_state.get(), rs);
        self.ready_state.set(rs);
        let event = Event::new(
            &self.global(),
            atom!("readystatechange"),
            EventBubbles::DoesNotBubble,
            EventCancelable::Cancelable,
        );
        event.fire(self.upcast());
    }

    fn process_headers_available(
        &self,
        gen_id: GenerationId,
        metadata: Result<FetchMetadata, NetworkError>,
    ) -> Result<(), Error> {
        let metadata = match metadata {
            Ok(meta) => match meta {
                FetchMetadata::Unfiltered(m) => m,
                FetchMetadata::Filtered { filtered, .. } => match filtered {
                    FilteredMetadata::Basic(m) => m,
                    FilteredMetadata::Cors(m) => m,
                    FilteredMetadata::Opaque => return Err(Error::Network),
                    FilteredMetadata::OpaqueRedirect => return Err(Error::Network),
                },
            },
            Err(_) => {
                self.process_partial_response(XHRProgress::Errored(gen_id, Error::Network));
                return Err(Error::Network);
            },
        };

        *self.response_url.borrow_mut() = metadata.final_url[..Position::AfterQuery].to_owned();

        // XXXManishearth Clear cache entries in case of a network error
        self.process_partial_response(XHRProgress::HeadersReceived(
            gen_id,
            metadata.headers.map(Serde::into_inner),
            metadata.status,
        ));
        Ok(())
    }

    fn process_data_available(&self, gen_id: GenerationId, payload: Vec<u8>) {
        self.process_partial_response(XHRProgress::Loading(gen_id, payload));
    }

    fn process_response_complete(
        &self,
        gen_id: GenerationId,
        status: Result<(), NetworkError>,
    ) -> ErrorResult {
        match status {
            Ok(()) => {
                self.process_partial_response(XHRProgress::Done(gen_id));
                Ok(())
            },
            Err(_) => {
                self.process_partial_response(XHRProgress::Errored(gen_id, Error::Network));
                Err(Error::Network)
            },
        }
    }

    fn process_partial_response(&self, progress: XHRProgress) {
        let msg_id = progress.generation_id();

        // Aborts processing if abort() or open() was called
        // (including from one of the event handlers called below)
        macro_rules! return_if_fetch_was_terminated(
            () => (
                if msg_id != self.generation_id.get() {
                    return
                }
            );
        );

        // Ignore message if it belongs to a terminated fetch
        return_if_fetch_was_terminated!();

        // Ignore messages coming from previously-errored responses or requests that have timed out
        if self.response_status.get().is_err() {
            return;
        }

        match progress {
            XHRProgress::HeadersReceived(_, headers, status) => {
                assert!(self.ready_state.get() == XMLHttpRequestState::Opened);
                // For synchronous requests, this should not fire any events, and just store data
                // XXXManishearth Find a way to track partial progress of the send (onprogresss for XHRUpload)

                // Part of step 13, send() (processing request end of file)
                // Substep 1
                self.upload_complete.set(true);
                // Substeps 2-4
                if !self.sync.get() {
                    self.dispatch_upload_progress_event(atom!("progress"), None);
                    return_if_fetch_was_terminated!();
                    self.dispatch_upload_progress_event(atom!("load"), None);
                    return_if_fetch_was_terminated!();
                    self.dispatch_upload_progress_event(atom!("loadend"), None);
                    return_if_fetch_was_terminated!();
                }
                // Part of step 13, send() (processing response)
                // XXXManishearth handle errors, if any (substep 1)
                // Substep 2
                status.map(|(code, reason)| {
                    self.status.set(code);
                    *self.status_text.borrow_mut() = ByteString::new(reason);
                });
                headers
                    .as_ref()
                    .map(|h| *self.response_headers.borrow_mut() = h.clone());
                {
                    let len = headers.and_then(|h| h.typed_get::<ContentLength>());
                    let mut response = self.response.borrow_mut();
                    response.clear();
                    if let Some(len) = len {
                        // don't attempt to prereserve more than 4 MB of memory,
                        // to avoid giving servers the ability to DOS the client by
                        // providing arbitrarily large content-lengths.
                        //
                        // this number is arbitrary, it's basically big enough that most
                        // XHR requests won't hit it, but not so big that it allows for DOS
                        let size = cmp::min(0b100_0000000000_0000000000, len.0 as usize);

                        // preallocate the buffer
                        response.reserve(size);
                    }
                }
                // Substep 3
                if !self.sync.get() {
                    self.change_ready_state(XMLHttpRequestState::HeadersReceived);
                }
            },
            XHRProgress::Loading(_, mut partial_response) => {
                // For synchronous requests, this should not fire any events, and just store data
                // Part of step 11, send() (processing response body)
                // XXXManishearth handle errors, if any (substep 2)

                self.response.borrow_mut().append(&mut partial_response);
                if !self.sync.get() {
                    if self.ready_state.get() == XMLHttpRequestState::HeadersReceived {
                        self.ready_state.set(XMLHttpRequestState::Loading);
                    }
                    let event = Event::new(
                        &self.global(),
                        atom!("readystatechange"),
                        EventBubbles::DoesNotBubble,
                        EventCancelable::Cancelable,
                    );
                    event.fire(self.upcast());
                    return_if_fetch_was_terminated!();
                    self.dispatch_response_progress_event(atom!("progress"));
                }
            },
            XHRProgress::Done(_) => {
                assert!(
                    self.ready_state.get() == XMLHttpRequestState::HeadersReceived ||
                        self.ready_state.get() == XMLHttpRequestState::Loading ||
                        self.sync.get()
                );

                self.cancel_timeout();
                self.canceller.borrow_mut().ignore();

                // Part of step 11, send() (processing response end of file)
                // XXXManishearth handle errors, if any (substep 2)

                // Subsubsteps 6-8
                self.send_flag.set(false);

                self.change_ready_state(XMLHttpRequestState::Done);
                return_if_fetch_was_terminated!();
                // Subsubsteps 11-12
                self.dispatch_response_progress_event(atom!("load"));
                return_if_fetch_was_terminated!();
                self.dispatch_response_progress_event(atom!("loadend"));
            },
            XHRProgress::Errored(_, e) => {
                self.cancel_timeout();
                self.canceller.borrow_mut().ignore();

                self.discard_subsequent_responses();
                self.send_flag.set(false);
                // XXXManishearth set response to NetworkError
                self.change_ready_state(XMLHttpRequestState::Done);
                return_if_fetch_was_terminated!();

                let errormsg = match e {
                    Error::Abort => "abort",
                    Error::Timeout => "timeout",
                    _ => "error",
                };

                let upload_complete = &self.upload_complete;
                if !upload_complete.get() {
                    upload_complete.set(true);
                    self.dispatch_upload_progress_event(Atom::from(errormsg), None);
                    return_if_fetch_was_terminated!();
                    self.dispatch_upload_progress_event(atom!("loadend"), None);
                    return_if_fetch_was_terminated!();
                }
                self.dispatch_response_progress_event(Atom::from(errormsg));
                return_if_fetch_was_terminated!();
                self.dispatch_response_progress_event(atom!("loadend"));
            },
        }
    }

    fn terminate_ongoing_fetch(&self) {
        self.canceller.borrow_mut().cancel();
        let GenerationId(prev_id) = self.generation_id.get();
        self.generation_id.set(GenerationId(prev_id + 1));
        self.response_status.set(Ok(()));
    }

    fn dispatch_progress_event(&self, upload: bool, type_: Atom, loaded: u64, total: Option<u64>) {
        let (total_length, length_computable) = if self
            .response_headers
            .borrow()
            .contains_key(header::CONTENT_ENCODING)
        {
            (0, false)
        } else {
            (total.unwrap_or(0), total.is_some())
        };
        let progressevent = ProgressEvent::new(
            &self.global(),
            type_,
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
            length_computable,
            loaded,
            total_length,
        );
        let target = if upload {
            self.upload.upcast()
        } else {
            self.upcast()
        };
        progressevent.upcast::<Event>().fire(target);
    }

    fn dispatch_upload_progress_event(&self, type_: Atom, partial_load: Option<u64>) {
        // If partial_load is None, loading has completed and we can just use the value from the request body

        let total = self.request_body_len.get() as u64;
        self.dispatch_progress_event(true, type_, partial_load.unwrap_or(total), Some(total));
    }

    fn dispatch_response_progress_event(&self, type_: Atom) {
        let len = self.response.borrow().len() as u64;
        let total = self
            .response_headers
            .borrow()
            .typed_get::<ContentLength>()
            .map(|v| v.0);
        self.dispatch_progress_event(false, type_, len, total);
    }

    fn set_timeout(&self, duration_ms: u32) {
        // Sets up the object to timeout in a given number of milliseconds
        // This will cancel all previous timeouts
        let callback = OneshotTimerCallback::XhrTimeout(XHRTimeoutCallback {
            xhr: Trusted::new(self),
            generation_id: self.generation_id.get(),
        });
        let duration = Length::new(duration_ms as u64);
        *self.timeout_cancel.borrow_mut() =
            Some(self.global().schedule_callback(callback, duration));
    }

    fn cancel_timeout(&self) {
        if let Some(handle) = self.timeout_cancel.borrow_mut().take() {
            self.global().unschedule_callback(handle);
        }
    }

    // https://xhr.spec.whatwg.org/#text-response
    fn text_response(&self) -> String {
        // Step 3, 5
        let charset = self.final_charset().unwrap_or(UTF_8);
        // TODO: Step 4 - add support for XML encoding guess stuff using XML spec

        // According to Simon, decode() should never return an error, so unwrap()ing
        // the result should be fine. XXXManishearth have a closer look at this later
        // Step 1, 2, 6
        let response = self.response.borrow();
        let (text, _, _) = charset.decode(&response);
        text.into_owned()
    }

    // https://xhr.spec.whatwg.org/#blob-response
    fn blob_response(&self) -> DomRoot<Blob> {
        // Step 1
        if let Some(response) = self.response_blob.get() {
            return response;
        }
        // Step 2
        let mime = self
            .final_mime_type()
            .as_ref()
            .map(|m| m.to_string())
            .unwrap_or("".to_owned());

        // Step 3, 4
        let bytes = self.response.borrow().to_vec();
        let blob = Blob::new(&self.global(), BlobImpl::new_from_bytes(bytes), mime);
        self.response_blob.set(Some(&blob));
        blob
    }

    // https://xhr.spec.whatwg.org/#arraybuffer-response
    #[allow(unsafe_code)]
    fn arraybuffer_response(&self, cx: JSContext) -> Option<NonNull<JSObject>> {
        // Step 1
        let created = self.response_arraybuffer.get();
        if let Some(nonnull) = NonNull::new(created) {
            return Some(nonnull);
        }

        // Step 2
        let bytes = self.response.borrow();
        rooted!(in(*cx) let mut array_buffer = ptr::null_mut::<JSObject>());
        unsafe {
            ArrayBuffer::create(*cx, CreateWith::Slice(&bytes), array_buffer.handle_mut())
                .ok()
                .and_then(|()| {
                    self.response_arraybuffer.set(array_buffer.get());
                    Some(NonNull::new_unchecked(array_buffer.get()))
                })
        }
    }

    // https://xhr.spec.whatwg.org/#document-response
    fn document_response(&self) -> Option<DomRoot<Document>> {
        // Caching: if we have existing response xml, redirect it directly
        let response = self.response_xml.get();
        if response.is_some() {
            return self.response_xml.get();
        }

        // Step 1
        if self.response_status.get().is_err() {
            return None;
        }

        let mime_type = self.final_mime_type();
        // TODO: prescan the response to determine encoding if final charset is null
        let charset = self.final_charset().unwrap_or(UTF_8);
        let temp_doc: DomRoot<Document>;
        match mime_type {
            Some(ref mime) if mime.type_() == mime::TEXT && mime.subtype() == mime::HTML => {
                // Step 5
                if self.response_type.get() == XMLHttpRequestResponseType::_empty {
                    return None;
                } else {
                    // Step 6
                    temp_doc = self.document_text_html();
                }
            },
            // Step 7
            Some(ref mime)
                if (mime.type_() == mime::TEXT && mime.subtype() == mime::XML) ||
                    (mime.type_() == mime::APPLICATION && mime.subtype() == mime::XML) =>
            {
                temp_doc = self.handle_xml();
            }
            None => {
                temp_doc = self.handle_xml();
            },
            Some(ref mime) if mime.suffix() == Some(mime::XML) => {
                temp_doc = self.handle_xml();
            },
            // Step 4
            _ => {
                return None;
            },
        }
        // Step 9
        temp_doc.set_encoding(charset);
        // Step 13
        self.response_xml.set(Some(&temp_doc));
        return self.response_xml.get();
    }

    #[allow(unsafe_code)]
    // https://xhr.spec.whatwg.org/#json-response
    fn json_response(&self, cx: JSContext) -> JSVal {
        // Step 1
        let response_json = self.response_json.get();
        if !response_json.is_null_or_undefined() {
            return response_json;
        }
        // Step 2
        let bytes = self.response.borrow();
        // Step 3
        if bytes.len() == 0 {
            return NullValue();
        }
        // Step 4
        fn decode_to_utf16_with_bom_removal(bytes: &[u8], encoding: &'static Encoding) -> Vec<u16> {
            let mut decoder = encoding.new_decoder_with_bom_removal();
            let capacity = decoder
                .max_utf16_buffer_length(bytes.len())
                .expect("Overflow");
            let mut utf16 = Vec::with_capacity(capacity);
            let extra = unsafe { slice::from_raw_parts_mut(utf16.as_mut_ptr(), capacity) };
            let last = true;
            let (_, read, written, _) = decoder.decode_to_utf16(bytes, extra, last);
            assert_eq!(read, bytes.len());
            unsafe { utf16.set_len(written) }
            utf16
        }
        // https://xhr.spec.whatwg.org/#json-response refers to
        // https://infra.spec.whatwg.org/#parse-json-from-bytes which refers to
        // https://encoding.spec.whatwg.org/#utf-8-decode which means
        // that the encoding is always UTF-8 and the UTF-8 BOM is removed,
        // if present, but UTF-16BE/LE BOM must not be honored.
        let json_text = decode_to_utf16_with_bom_removal(&bytes, UTF_8);
        // Step 5
        rooted!(in(*cx) let mut rval = UndefinedValue());
        unsafe {
            if !JS_ParseJSON(
                *cx,
                json_text.as_ptr(),
                json_text.len() as u32,
                rval.handle_mut(),
            ) {
                JS_ClearPendingException(*cx);
                return NullValue();
            }
        }
        // Step 6
        self.response_json.set(rval.get());
        self.response_json.get()
    }

    fn document_text_html(&self) -> DomRoot<Document> {
        let charset = self.final_charset().unwrap_or(UTF_8);
        let wr = self.global();
        let response = self.response.borrow();
        let (decoded, _, _) = charset.decode(&response);
        let document = self.new_doc(IsHTMLDocument::HTMLDocument);
        // TODO: Disable scripting while parsing
        ServoParser::parse_html_document(&document, DOMString::from(decoded), wr.get_url());
        document
    }

    fn handle_xml(&self) -> DomRoot<Document> {
        let charset = self.final_charset().unwrap_or(UTF_8);
        let wr = self.global();
        let response = self.response.borrow();
        let (decoded, _, _) = charset.decode(&response);
        let document = self.new_doc(IsHTMLDocument::NonHTMLDocument);
        // TODO: Disable scripting while parsing
        ServoParser::parse_xml_document(&document, DOMString::from(decoded), wr.get_url());
        document
    }

    fn new_doc(&self, is_html_document: IsHTMLDocument) -> DomRoot<Document> {
        let wr = self.global();
        let win = wr.as_window();
        let doc = win.Document();
        let docloader = DocumentLoader::new(&*doc.loader());
        let base = wr.get_url();
        let parsed_url = match base.join(&self.ResponseURL().0) {
            Ok(parsed) => Some(parsed),
            Err(_) => None, // Step 7
        };
        let content_type = self.final_mime_type();
        Document::new(
            win,
            HasBrowsingContext::No,
            parsed_url,
            doc.origin().clone(),
            is_html_document,
            content_type,
            None,
            DocumentActivity::Inactive,
            DocumentSource::FromParser,
            docloader,
            None,
            None,
            Default::default(),
        )
    }

    fn filter_response_headers(&self) -> HeaderMap {
        // https://fetch.spec.whatwg.org/#concept-response-header-list
        let mut headers = self.response_headers.borrow().clone();
        headers.remove(header::SET_COOKIE);
        headers.remove(HeaderName::from_static("set-cookie2"));
        // XXXManishearth additional CORS filtering goes here
        headers
    }

    fn discard_subsequent_responses(&self) {
        self.response_status.set(Err(()));
    }

    fn fetch(&self, init: RequestBuilder, global: &GlobalScope) -> ErrorResult {
        let xhr = Trusted::new(self);

        let context = Arc::new(Mutex::new(XHRContext {
            xhr: xhr,
            gen_id: self.generation_id.get(),
            sync_status: DomRefCell::new(None),
            resource_timing: ResourceFetchTiming::new(ResourceTimingType::Resource),
            url: init.url.clone(),
        }));

        let (task_source, script_port) = if self.sync.get() {
            let (tx, rx) = global.new_script_pair();
            (NetworkingTaskSource(tx, global.pipeline_id()), Some(rx))
        } else {
            (global.networking_task_source(), None)
        };

        let cancel_receiver = self.canceller.borrow_mut().initialize();

        XMLHttpRequest::initiate_async_xhr(
            context.clone(),
            task_source,
            global,
            init,
            cancel_receiver,
        );

        if let Some(script_port) = script_port {
            loop {
                global.process_event(script_port.recv().unwrap());
                let context = context.lock().unwrap();
                let sync_status = context.sync_status.borrow();
                if let Some(ref status) = *sync_status {
                    return status.clone();
                }
            }
        }
        Ok(())
    }

    fn final_charset(&self) -> Option<&'static Encoding> {
        if self.override_charset.borrow().is_some() {
            self.override_charset.borrow().clone()
        } else {
            match self.response_headers.borrow().typed_get::<ContentType>() {
                Some(ct) => {
                    let mime: Mime = ct.into();
                    let value = mime.get_param(mime::CHARSET);
                    value.and_then(|value| Encoding::for_label(value.as_ref().as_bytes()))
                },
                None => None,
            }
        }
    }

    fn final_mime_type(&self) -> Option<Mime> {
        if self.override_mime_type.borrow().is_some() {
            self.override_mime_type.borrow().clone()
        } else {
            match self.response_headers.borrow().typed_get::<ContentType>() {
                Some(ct) => Some(ct.into()),
                None => None,
            }
        }
    }
}

#[derive(JSTraceable, MallocSizeOf)]
pub struct XHRTimeoutCallback {
    #[ignore_malloc_size_of = "Because it is non-owning"]
    xhr: Trusted<XMLHttpRequest>,
    generation_id: GenerationId,
}

impl XHRTimeoutCallback {
    pub fn invoke(self) {
        let xhr = self.xhr.root();
        if xhr.ready_state.get() != XMLHttpRequestState::Done {
            xhr.process_partial_response(XHRProgress::Errored(self.generation_id, Error::Timeout));
        }
    }
}

pub trait Extractable {
    fn extract(&self) -> (Vec<u8>, Option<DOMString>);
}

impl Extractable for Blob {
    fn extract(&self) -> (Vec<u8>, Option<DOMString>) {
        let content_type = if self.Type().as_ref().is_empty() {
            None
        } else {
            Some(self.Type())
        };
        let bytes = self.get_bytes().unwrap_or(vec![]);
        (bytes, content_type)
    }
}

impl Extractable for DOMString {
    fn extract(&self) -> (Vec<u8>, Option<DOMString>) {
        (
            self.as_bytes().to_owned(),
            Some(DOMString::from("text/plain;charset=UTF-8")),
        )
    }
}

impl Extractable for FormData {
    fn extract(&self) -> (Vec<u8>, Option<DOMString>) {
        let boundary = generate_boundary();
        let bytes = encode_multipart_form_data(&mut self.datums(), boundary.clone(), UTF_8);
        (
            bytes,
            Some(DOMString::from(format!(
                "multipart/form-data;boundary={}",
                boundary
            ))),
        )
    }
}

impl Extractable for URLSearchParams {
    fn extract(&self) -> (Vec<u8>, Option<DOMString>) {
        (
            self.serialize_utf8().into_bytes(),
            Some(DOMString::from(
                "application/x-www-form-urlencoded;charset=UTF-8",
            )),
        )
    }
}

fn serialize_document(doc: &Document) -> Fallible<DOMString> {
    let mut writer = vec![];
    match serialize(&mut writer, &doc.upcast::<Node>(), SerializeOpts::default()) {
        Ok(_) => Ok(DOMString::from(String::from_utf8(writer).unwrap())),
        Err(_) => Err(Error::InvalidState),
    }
}

impl Extractable for BodyInit {
    // https://fetch.spec.whatwg.org/#concept-bodyinit-extract
    fn extract(&self) -> (Vec<u8>, Option<DOMString>) {
        match *self {
            BodyInit::String(ref s) => s.extract(),
            BodyInit::URLSearchParams(ref usp) => usp.extract(),
            BodyInit::Blob(ref b) => b.extract(),
            BodyInit::FormData(ref formdata) => formdata.extract(),
            BodyInit::ArrayBuffer(ref typedarray) => ((typedarray.to_vec(), None)),
            BodyInit::ArrayBufferView(ref typedarray) => ((typedarray.to_vec(), None)),
        }
    }
}

/// Returns whether `bs` is a `field-value`, as defined by
/// [RFC 2616](http://tools.ietf.org/html/rfc2616#page-32).
pub fn is_field_value(slice: &[u8]) -> bool {
    // Classifications of characters necessary for the [CRLF] (SP|HT) rule
    #[derive(PartialEq)]
    enum PreviousCharacter {
        Other,
        CR,
        LF,
        SPHT, // SP or HT
    }
    let mut prev = PreviousCharacter::Other; // The previous character
    slice.iter().all(|&x| {
        // http://tools.ietf.org/html/rfc2616#section-2.2
        match x {
            13 => {
                // CR
                if prev == PreviousCharacter::Other || prev == PreviousCharacter::SPHT {
                    prev = PreviousCharacter::CR;
                    true
                } else {
                    false
                }
            },
            10 => {
                // LF
                if prev == PreviousCharacter::CR {
                    prev = PreviousCharacter::LF;
                    true
                } else {
                    false
                }
            },
            32 => {
                // SP
                if prev == PreviousCharacter::LF || prev == PreviousCharacter::SPHT {
                    prev = PreviousCharacter::SPHT;
                    true
                } else if prev == PreviousCharacter::Other {
                    // Counts as an Other here, since it's not preceded by a CRLF
                    // SP is not a CTL, so it can be used anywhere
                    // though if used immediately after a CR the CR is invalid
                    // We don't change prev since it's already Other
                    true
                } else {
                    false
                }
            },
            9 => {
                // HT
                if prev == PreviousCharacter::LF || prev == PreviousCharacter::SPHT {
                    prev = PreviousCharacter::SPHT;
                    true
                } else {
                    false
                }
            },
            0..=31 | 127 => false, // CTLs
            x if x > 127 => false, // non ASCII
            _ if prev == PreviousCharacter::Other || prev == PreviousCharacter::SPHT => {
                prev = PreviousCharacter::Other;
                true
            },
            _ => false, // Previous character was a CR/LF but not part of the [CRLF] (SP|HT) rule
        }
    })
}
