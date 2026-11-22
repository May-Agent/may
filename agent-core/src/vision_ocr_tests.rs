//! The `vision_ocr` integration test that needs `may-agent`'s `ChildHooks`:
//! a scripted child agent whose tool calls record images the hooks attach to
//! `Message::Tool`, with OCR and conditional downscaling applied.
//!
//! The fixture images come from `may-media`'s `testing` feature, enabled for
//! test builds by this crate's `[dev-dependencies]`.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use aither_agent::Agent;
use aither_core::llm::model::Profile;
use aither_core::llm::{
    Classify, Event, Failure, LLMRequest, LanguageModel, Role, Tool, ToolCall, ToolCallId,
    ToolContext, ToolResult,
};
use futures_lite::Stream;
use image::GenericImageView;
use may_media::test_support::{make_photo_png, render_multilingual_text_png};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::ipc::{CallImageStore, RecordedCallImage};
use crate::subagent::ChildHooks;
use crate::vision_ocr::{OCR_DOWNSCALE_MAX_DIM, VisionOcr};

#[derive(Debug)]
struct StubErr;
impl std::fmt::Display for StubErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stub error")
    }
}
impl std::error::Error for StubErr {}
impl Classify for StubErr {
    fn failure(&self) -> Failure {
        Failure::permanent()
    }
}

#[derive(Clone)]
struct ScriptedModel {
    script: Arc<Mutex<VecDeque<Vec<Event>>>>,
    requests: Arc<Mutex<Vec<LLMRequest>>>,
}

impl LanguageModel for ScriptedModel {
    type Error = StubErr;

    fn respond(
        &self,
        request: LLMRequest,
    ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
        self.requests.lock().expect("requests").push(request);
        let events = self
            .script
            .lock()
            .expect("script")
            .pop_front()
            .expect("scripted turn");
        futures_lite::stream::iter(events.into_iter().map(Ok))
    }

    fn profile(&self) -> impl std::future::Future<Output = Result<Profile, Self::Error>> + Send {
        std::future::ready(Ok(Profile::new(
            "scripted",
            "test",
            "scripted",
            "scripted model",
            128_000,
        )))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NoArgs {}

struct CaptureTool {
    store: CallImageStore,
    images: Vec<RecordedCallImage>,
}

impl Tool for CaptureTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("terminal")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed("Captures a test screenshot.")
    }

    type Arguments = NoArgs;
    type Res = ToolResult;

    fn call(
        &self,
        _args: Self::Arguments,
        cx: ToolContext,
    ) -> impl std::future::Future<Output = aither_core::Result<Self::Res>> + Send {
        if let Some(call) = cx.identity() {
            self.store.record(call.call(), self.images.clone());
        }
        std::future::ready(Ok(ToolResult::text("Screenshot captured.")))
    }
}

#[tokio::test]
async fn child_hooks_forward_ocr_and_downscaled_images() {
    let dir = tempfile::tempdir().expect("tempdir");
    let screenshot_path = dir.path().join("screen.png");
    let photo_path = dir.path().join("photo.png");
    let screenshot = render_multilingual_text_png(
        &[
            "Build status: 12 targets succeeded",
            "中文界面状态：所有编译与集成测试全部通过",
            "日本語確認：ビルド成功しました",
            "한국어 확인: 배포 준비 완료",
        ],
        1200,
        600,
    );
    let photo = make_photo_png(1200, 800);
    async_fs::write(&screenshot_path, &screenshot)
        .await
        .expect("write screenshot");
    async_fs::write(&photo_path, &photo)
        .await
        .expect("write photo");

    let ocr = VisionOcr::new();
    let store = CallImageStore::default();
    let outputs_dir = dir.path().join("outputs");
    let hooks = ChildHooks::compacting(None, Arc::new(AtomicBool::new(false))).with_image_ocr(
        ocr.clone(),
        store.clone(),
        outputs_dir.clone(),
    );
    let recorded = vec![
        RecordedCallImage {
            path: screenshot_path.clone(),
            media_type: "image/png".to_owned(),
            source: "computer screenshot".to_owned(),
        },
        RecordedCallImage {
            path: photo_path.clone(),
            media_type: "image/png".to_owned(),
            source: "browser screenshot".to_owned(),
        },
    ];
    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm = ScriptedModel {
        script: Arc::new(Mutex::new(VecDeque::from([
            vec![Event::ToolCall(ToolCall::new(
                "call-recorded",
                "terminal",
                serde_json::json!({}),
            ))],
            vec![Event::Text("done".to_owned())],
        ]))),
        requests: Arc::clone(&requests),
    };
    let mut agent = Agent::builder(llm)
        .tool(CaptureTool {
            store: store.clone(),
            images: recorded,
        })
        .hooks(hooks)
        .build();
    let _ = agent
        .run("inspect", std::iter::empty())
        .drive((), std::future::pending::<()>(), |(), _| async {
            ControlFlow::Continue(())
        })
        .await
        .expect("child agent completes");

    let tool_message = {
        let captured_requests = requests.lock().expect("requests");
        captured_requests[1]
            .messages()
            .iter()
            .find(|msg| msg.role() == Role::Tool && msg.tool_call_id() == Some("call-recorded"))
            .expect("tool message reaches model")
            .clone()
    };
    let attachments = tool_message.attachments();
    assert_eq!(attachments.len(), 2);
    let rendered = tool_message.content();
    assert!(rendered.contains("Screenshot captured."), "{rendered}");
    assert!(
        rendered.contains("<recognized_text source=\"computer screenshot\">"),
        "{rendered}"
    );
    let raw_lines = || ocr.recognize_lines(&screenshot);
    assert!(
        rendered.contains("中文"),
        "{rendered}; raw={:?}",
        raw_lines()
    );
    assert!(
        rendered.contains("日本語"),
        "{rendered}; raw={:?}",
        raw_lines()
    );
    assert!(
        rendered.contains("한국어"),
        "{rendered}; raw={:?}",
        raw_lines()
    );

    let resized_path = attachments[0].url().to_file_path().expect("resized file");
    assert_ne!(
        resized_path,
        async_fs::canonicalize(&screenshot_path)
            .await
            .expect("canon screenshot")
    );
    let resized_bytes = async_fs::read(&resized_path).await.expect("read resized");
    let (w, h) = image::load_from_memory(&resized_bytes)
        .expect("decode resized")
        .dimensions();
    assert!(w <= OCR_DOWNSCALE_MAX_DIM && h <= OCR_DOWNSCALE_MAX_DIM);

    let unchanged_path = attachments[1].url().to_file_path().expect("photo file");
    assert_eq!(
        unchanged_path,
        async_fs::canonicalize(&photo_path)
            .await
            .expect("canon photo")
    );
    assert!(
        store.take(&ToolCallId::new("call-recorded")).is_empty(),
        "store entry was consumed"
    );
}
