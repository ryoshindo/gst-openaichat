use std::{
  env, str,
  sync::{Arc, Mutex},
};

use gstreamer::{
  glib::{self, ParamSpec, Value},
  prelude::{GstParamSpecBuilderExt, PadExt, ParamSpecBuilderExt, ToValue},
  subclass::{
    prelude::{ElementImpl, GstObjectImpl, ObjectImpl, ObjectSubclass, ObjectSubclassExt},
    ElementMetadata,
  },
  Buffer, Caps, CapsIntersectMode, DebugCategory, ErrorMessage, FlowError, PadDirection,
  PadPresence, PadTemplate,
};
use gstreamer_base::{
  prelude::BaseTransformExtManual,
  subclass::{
    base_transform::{BaseTransformImpl, BaseTransformImplExt, GenerateOutputSuccess},
    BaseTransformMode,
  },
  BaseTransform,
};
use hyper::{client::HttpConnector, Method, Request};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use once_cell::sync::Lazy;
use tokio::runtime::{self, Runtime};

use crate::filter::openai_model::{
  OpenAiChatCompletionResponse, OpenaiChatCompletionMessage, OpenaiChatCompletionRequest,
};

const DEFAULT_MODEL: &str = "gpt-3.5-turbo";

static CAT: Lazy<DebugCategory> = Lazy::new(|| {
  DebugCategory::new(
    "openaichat",
    gstreamer::DebugColorFlags::empty(),
    Some("Text to text filter using the OpenAI Chat API"),
  )
});

static CAPS: Lazy<Caps> = Lazy::new(|| Caps::builder("text/x-raw").field("format", "utf8").build());

static RUNTIME: Lazy<Runtime> = Lazy::new(|| {
  runtime::Builder::new_multi_thread()
    .enable_all()
    .worker_threads(1)
    .build()
    .unwrap()
});

static HTTPS_CLIENT: Lazy<hyper::Client<HttpsConnector<HttpConnector>>> = Lazy::new(|| {
  let https = HttpsConnectorBuilder::new()
    .with_native_roots()
    .https_only()
    .enable_all_versions()
    .build();
  hyper::Client::builder().build(https)
});

static OPENAI_API_KEY: Lazy<String> =
  Lazy::new(|| env::var("OPENAI_API_KEY").expect("missing OPENAI_API_KEY environment variable"));

static OPENAI_ENDPOINT: Lazy<String> = 
  Lazy::new(|| env::var("OPENAI_ENDPOINT").unwrap_or("https://api.openai.com/v1/chat/completions".to_string()));

#[derive(Debug, Clone, Default)]
struct Settings {
  model: String,
}

#[derive(Default, Debug)]
struct State {
  history: Vec<OpenaiChatCompletionMessage>,
}

pub struct OpenaiChatFilter {
  #[allow(dead_code)]
  settings: Mutex<Settings>,
  state: Arc<Mutex<State>>,
}

#[glib::object_subclass]
impl ObjectSubclass for OpenaiChatFilter {
  type ParentType = BaseTransform;
  type Type = super::OpenaiChatFilter;

  const NAME: &'static str = "GstOpenaiChatFilter";

  fn new() -> Self {
    Self {
      settings: Mutex::new(Settings {
        model: DEFAULT_MODEL.into(),
      }),
      state: Arc::new(Mutex::new(Default::default())),
    }
  }
}

impl ObjectImpl for OpenaiChatFilter {
  fn properties() -> &'static [ParamSpec] {
    static PROPERTIES: Lazy<Vec<ParamSpec>> = Lazy::new(|| {
      vec![
      glib::ParamSpecString::builder("model")
        .nick("Model")
        .blurb(&format!("The OpenAI model to use. Defaults to {}. Possible values are listed at https://platform.openai.com/docs/models/model-endpoint-compatibility", DEFAULT_MODEL))
        .mutable_ready()
        .mutable_paused()
        .mutable_playing()
        .build(),
    ]
    });
    PROPERTIES.as_ref()
  }

  fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
    let mut settings = self.settings.lock().unwrap();
    match pspec.name() {
      "model" => {
        settings.model = value.get().unwrap();
      },
      other => panic!("no such property: {}", other),
    }
  }

  fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
    match pspec.name() {
      "model" => {
        let settings = self.settings.lock().unwrap();
        settings.model.to_value()
      },
      other => panic!("no such property: {}", other),
    }
  }
}

impl GstObjectImpl for OpenaiChatFilter {}

impl ElementImpl for OpenaiChatFilter {
  fn metadata() -> Option<&'static ElementMetadata> {
    static ELEMENT_METADATA: Lazy<ElementMetadata> = Lazy::new(|| {
      ElementMetadata::new(
        "OpenAI Chat API element",
        "Effect/Text",
        "Sink a text buffer, send it to the OpenAI Chat API, and source the response as a text buffer",
        "Jasper Hugo <jasper@avstack.io>",
      )
    });

    Some(&*ELEMENT_METADATA)
  }

  fn pad_templates() -> &'static [PadTemplate] {
    static PAD_TEMPLATES: Lazy<Vec<PadTemplate>> = Lazy::new(|| {
      let src_pad_template =
        PadTemplate::new("src", PadDirection::Src, PadPresence::Always, &CAPS).unwrap();

      let sink_pad_template = gstreamer::PadTemplate::new(
        "sink",
        gstreamer::PadDirection::Sink,
        gstreamer::PadPresence::Always,
        &CAPS,
      )
      .unwrap();

      vec![src_pad_template, sink_pad_template]
    });

    PAD_TEMPLATES.as_ref()
  }
}

impl BaseTransformImpl for OpenaiChatFilter {
  const MODE: BaseTransformMode = BaseTransformMode::NeverInPlace;
  const PASSTHROUGH_ON_SAME_CAPS: bool = false;
  const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

  fn start(&self) -> Result<(), ErrorMessage> {
    gstreamer::debug!(CAT, "start()");
    Ok(())
  }

  fn stop(&self) -> Result<(), ErrorMessage> {
    gstreamer::debug!(CAT, "stop()");
    Ok(())
  }

  fn transform_caps(
    &self,
    _direction: PadDirection,
    _caps: &Caps,
    maybe_filter: Option<&Caps>,
  ) -> Option<Caps> {
    let mut caps = CAPS.clone();
    if let Some(filter) = maybe_filter {
      caps = filter.intersect_with_mode(&caps, CapsIntersectMode::First);
    }
    Some(caps)
  }

  fn generate_output(&self) -> Result<GenerateOutputSuccess, FlowError> {
    if let Some(buffer) = self.take_queued_buffer() {
      let src_pad = self.obj().src_pad().to_owned();

      let buffer_reader = buffer.as_ref().map_readable().unwrap();

      let content = str::from_utf8(buffer_reader.as_slice()).unwrap();

      let messages = {
        let mut state = self.state.lock().unwrap();
        state.history.push(OpenaiChatCompletionMessage {
          role: "system".into(),
          content: "
            あなたは陽気な高校生の患者で、現在は病院に来て発熱外来の診察をしています。本当にしんどい発熱なので、すごくつらそうです。
            返答は一言もしくは二言程度でお願いします。
            医者と話す際は基本的に語尾には「のだ。」や「なのだ。」を付けてください。
            そして一人称は「ボク」とし、必ず各文章に一人称をつけてください。
            少し怒りっぽく、子供っぽいところもあります。

            「ありがとうございます」等の感謝の言葉を受け取ったら診察は終了です。
            今回の診察における新米医者に対するフィードバックをベテラン医者の立場で行ってください。ただし、口調は上記の患者ですが、詳細なフィードバックをしてください。
            フィードバックに対しての質問が来た場合は、その質問に対しても答えてください。
            「ありがとうございます」等の感謝の言葉を受け取ったらフィードバックは終了です。
          ".into(),
        });
        state.history.push(OpenaiChatCompletionMessage {
          role: "user".into(),
          content: content.to_string().into(),
        });
        state.history.clone()
      };

      let request_body = OpenaiChatCompletionRequest {
        model: "gpt-3.5-turbo".into(),
        messages,
      };

      let state = self.state.clone();

      RUNTIME.spawn(async move {
        let request = Request::builder()
          .method(Method::POST)
          .uri(format!("{}", *OPENAI_ENDPOINT))
          .header("api-key", format!("{}", *OPENAI_API_KEY))
          .header("Content-Type", "application/json")
          .body(serde_json::to_vec(&request_body).unwrap().into())
          .unwrap();
        let response = HTTPS_CLIENT.request(request).await.unwrap();
        if response.status().is_success() {
          let response_body = hyper::body::to_bytes(response).await.unwrap();
          let response_body: OpenAiChatCompletionResponse =
            serde_json::from_slice(&response_body).unwrap();
          let message = &response_body.choices[0].message;
          state.lock().unwrap().history.push(message.clone());
          let content = format!("{}\n", message.content);
          let mut buffer = Buffer::with_size(content.len()).unwrap();
          buffer
            .get_mut()
            .unwrap()
            .copy_from_slice(0, content.as_bytes())
            .unwrap();
          src_pad.push(buffer).unwrap();
        }
        else {
          gstreamer::debug!(CAT, "HTTP error from OpenAI API: {}", response.status());
        }
      });

      Ok(GenerateOutputSuccess::NoOutput)
    }
    else {
      gstreamer::debug!(CAT, "generate_output(): no queued buffers to take");
      Ok(GenerateOutputSuccess::NoOutput)
    }
  }
}
