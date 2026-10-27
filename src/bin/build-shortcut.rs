//! Builds the "ixian" Apple Shortcut (macOS only: needs `plutil` + `shortcuts sign`).
//!
//!   cargo run --bin build-shortcut
//!       -> target/ixian.shortcut, asks server/id/secret on import
//!   cargo run --bin build-shortcut -- --server URL --id ID --secret S --out x.shortcut
//!       -> values baked in, no import questions
//!   add --test to output the video instead of showing the menu
//!       (for `shortcuts run NAME -i in.txt -o out.mp4`)
use serde_json::{json, Map, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

const DEFAULT_SERVER: &str = "https://ixian.astrapolaris.studio";
const IS: u8 = 4;
const HAS_NO_VALUE: u8 = 101;

/// A value the workflow can reference: the shortcut input or an earlier action's output.
enum Target {
    ExtensionInput,
    Action { uuid: String, name: String },
}

impl Target {
    fn out(&self) -> Value {
        match self {
            Target::ExtensionInput => json!({ "Type": "ExtensionInput" }),
            Target::Action { uuid, name } => json!({
                "OutputUUID": uuid,
                "OutputName": name,
                "Type": "ActionOutput",
            }),
        }
    }

    fn attachment(&self) -> Value {
        json!({
            "Value": self.out(),
            "WFSerializationType": "WFTextTokenAttachment",
        })
    }
}

#[derive(Clone, Copy)]
enum Token<'a> {
    Text(&'a str),
    Ref(&'a Target),
}

/// Text with embedded variables, as a `WFTextTokenString`.
fn text_token(parts: &[Token]) -> Value {
    let mut text = String::new();
    let mut attachments = Map::new();
    for part in parts {
        match part {
            Token::Text(value) => text.push_str(value),
            Token::Ref(target) => {
                let offset = text.encode_utf16().count();
                attachments.insert(format!("{{{offset}, 1}}"), target.out());
                text.push('\u{fffc}');
            }
        }
    }
    json!({
        "Value": {
            "string": text,
            "attachmentsByRange": attachments,
        },
        "WFSerializationType": "WFTextTokenString",
    })
}

/// A workflow dictionary whose values may embed variables.
fn dictionary(entries: &[(&str, Token)]) -> Value {
    let items: Vec<Value> = entries
        .iter()
        .map(|(key, value)| {
            json!({
                "WFItemType": 0,
                "WFKey": text_token(&[Token::Text(key)]),
                "WFValue": text_token(&[*value]),
            })
        })
        .collect();
    json!({
        "Value": { "WFDictionaryFieldValueItems": items },
        "WFSerializationType": "WFDictionaryFieldValue",
    })
}

struct Shortcut {
    counter: u32,
    actions: Vec<Value>,
    questions: Vec<Value>,
}

impl Shortcut {
    fn new() -> Self {
        Self {
            counter: 0,
            actions: Vec::new(),
            questions: Vec::new(),
        }
    }

    fn next_uuid(&mut self) -> String {
        self.counter += 1;
        format!("1A1A0000-0000-4000-8000-{:012}", self.counter)
    }

    fn action(&mut self, id: &str, params: Map<String, Value>) -> Target {
        self.action_named(id, params, "")
    }

    fn action_named(&mut self, id: &str, params: Map<String, Value>, name: &str) -> Target {
        let uuid = self.next_uuid();
        let mut parameters = Map::new();
        parameters.insert("UUID".into(), Value::String(uuid.clone()));
        parameters.extend(params);
        self.actions.push(json!({
            "WFWorkflowActionIdentifier": format!("is.workflow.actions.{id}"),
            "WFWorkflowActionParameters": parameters,
        }));
        Target::Action {
            uuid,
            name: name.into(),
        }
    }

    /// Text action; when `value` is absent, asks for it on import (prefilled with `fallback`).
    fn text(&mut self, value: Option<&str>, question: &str, fallback: &str) -> Target {
        if value.is_none() {
            self.questions.push(json!({
                "ActionIndex": self.actions.len(),
                "Category": "Parameter",
                "ParameterKey": "WFTextActionText",
                "Text": question,
                "DefaultValue": fallback,
            }));
        }
        let mut params = Map::new();
        params.insert(
            "WFTextActionText".into(),
            Value::String(value.unwrap_or(fallback).into()),
        );
        params.insert("CustomOutputName".into(), Value::String(question.into()));
        self.action_named("gettext", params, question)
    }

    fn alert(&mut self, title: &str, message: Value) {
        let mut params = Map::new();
        params.insert("WFAlertActionTitle".into(), Value::String(title.into()));
        params.insert("WFAlertActionMessage".into(), message);
        params.insert("WFAlertActionCancelButtonShown".into(), Value::Bool(false));
        self.action("alert", params);
    }
}

fn conditional(group: &str, mode: u8, extra: Value) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert("GroupingIdentifier".into(), Value::String(group.into()));
    params.insert("WFControlFlowMode".into(), json!(mode));
    if let Value::Object(fields) = extra {
        params.extend(fields);
    }
    params
}

struct Args {
    server: Option<String>,
    id: Option<String>,
    secret: Option<String>,
    out: PathBuf,
    test: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut server = None;
        let mut id = None;
        let mut secret = None;
        let mut out = None;
        let mut test = false;
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| args.next().ok_or_else(|| format!("{flag} needs a value"));
            match arg.as_str() {
                "--server" => server = Some(value("--server")?),
                "--id" => id = Some(value("--id")?),
                "--secret" => secret = Some(value("--secret")?),
                "--out" => out = Some(PathBuf::from(value("--out")?)),
                "--test" => test = true,
                other => {
                    return Err(format!(
                    "unknown argument '{other}' (expected --server, --id, --secret, --out, --test)"
                ))
                }
            }
        }
        Ok(Self {
            server,
            id,
            secret,
            out: out.unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("target")
                    .join("ixian.shortcut")
            }),
            test,
        })
    }
}

fn run(command: &mut Command) -> Result<(), String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .map_err(|error| format!("failed to run {program}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    Err(if detail.is_empty() {
        format!("{program} failed ({})", output.status)
    } else {
        format!("{program} failed: {detail}")
    })
}

struct TempFiles(Vec<PathBuf>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn run_builder() -> Result<(), String> {
    let args = Args::parse()?;
    let mut sc = Shortcut::new();

    // --- the shortcut ---
    let server = sc.text(args.server.as_deref(), "ixian server URL", DEFAULT_SERVER);
    let id = sc.text(args.id.as_deref(), "CF-Access-Client-Id", "");
    let secret = sc.text(args.secret.as_deref(), "CF-Access-Client-Secret", "");

    // Shared input as text first: share sheets hand over URLs, text, or both.
    let mut params = Map::new();
    params.insert(
        "WFInput".into(),
        text_token(&[Token::Ref(&Target::ExtensionInput)]),
    );
    let urls = sc.action_named("detect.link", params, "URLs");

    let group = sc.next_uuid();
    sc.action(
        "conditional",
        conditional(
            &group,
            0,
            json!({
                "WFInput": { "Type": "Variable", "Variable": urls.attachment() },
                "WFCondition": HAS_NO_VALUE,
            }),
        ),
    );
    sc.alert(
        "ixian",
        text_token(&[Token::Text(
            "No link found. Share a YouTube, X or Instagram post.",
        )]),
    );
    sc.action("exit", Map::new());
    sc.action_named(
        "conditional",
        conditional(&group, 2, json!({})),
        "If Result",
    );

    let mut params = Map::new();
    params.insert("WFInput".into(), urls.attachment());
    params.insert("WFItemSpecifier".into(), json!("First Item"));
    let link = sc.action_named("getitemfromlist", params, "Item from List");

    let mut params = Map::new();
    params.insert(
        "WFURL".into(),
        text_token(&[Token::Ref(&server), Token::Text("/download")]),
    );
    params.insert("WFHTTPMethod".into(), json!("POST"));
    params.insert("Advanced".into(), json!(true));
    params.insert("ShowHeaders".into(), json!(true));
    params.insert(
        "WFHTTPHeaders".into(),
        dictionary(&[
            ("CF-Access-Client-Id", Token::Ref(&id)),
            ("CF-Access-Client-Secret", Token::Ref(&secret)),
        ]),
    );
    params.insert("WFHTTPBodyType".into(), json!("JSON"));
    params.insert(
        "WFJSONValues".into(),
        dictionary(&[("url", Token::Ref(&link))]),
    );
    let video = sc.action_named("downloadurl", params, "Contents of URL");

    let mut params = Map::new();
    params.insert("WFInput".into(), video.attachment());
    params.insert("WFContentItemPropertyName".into(), json!("File Extension"));
    let ext = sc.action_named("properties.files", params, "File Extension");

    let group = sc.next_uuid();
    sc.action(
        "conditional",
        conditional(
            &group,
            0,
            json!({
                "WFInput": { "Type": "Variable", "Variable": ext.attachment() },
                "WFCondition": IS,
                "WFConditionalActionString": "json",
            }),
        ),
    );
    let mut params = Map::new();
    params.insert("WFInput".into(), video.attachment());
    params.insert("WFDictionaryKey".into(), json!("error"));
    let error = sc.action_named("getvalueforkey", params, "Dictionary Value");
    sc.alert("ixian failed", text_token(&[Token::Ref(&error)]));
    sc.action("exit", Map::new());
    sc.action_named(
        "conditional",
        conditional(&group, 2, json!({})),
        "If Result",
    );

    let group = sc.next_uuid();
    sc.action(
        "conditional",
        conditional(
            &group,
            0,
            json!({
                "WFInput": { "Type": "Variable", "Variable": ext.attachment() },
                "WFCondition": IS,
                "WFConditionalActionString": "html",
            }),
        ),
    );
    sc.alert(
        "ixian unreachable",
        text_token(&[Token::Text(
            "Got a web page instead of a video. Check the server URL and the Cloudflare Access client id/secret.",
        )]),
    );
    sc.action("exit", Map::new());
    sc.action_named(
        "conditional",
        conditional(&group, 2, json!({})),
        "If Result",
    );

    if args.test {
        let mut params = Map::new();
        params.insert("WFVariable".into(), video.attachment());
        sc.action("getvariable", params);
    } else {
        let items = ["Save to Photos", "Share", "Save to Files"];
        let group = sc.next_uuid();
        let mut params = Map::new();
        params.insert("GroupingIdentifier".into(), Value::String(group.clone()));
        params.insert("WFControlFlowMode".into(), json!(0));
        params.insert("WFMenuPrompt".into(), json!("ixian"));
        params.insert("WFMenuItems".into(), json!(items));
        sc.action("choosefrommenu", params);

        let cases = [
            ("Save to Photos", "savetocameraroll"),
            ("Share", "share"),
            ("Save to Files", "documentpicker.save"),
        ];
        for (title, action_id) in cases {
            let mut params = Map::new();
            params.insert("GroupingIdentifier".into(), Value::String(group.clone()));
            params.insert("WFControlFlowMode".into(), json!(1));
            params.insert("WFMenuItemTitle".into(), json!(title));
            sc.action("choosefrommenu", params);

            let mut params = Map::new();
            params.insert("WFInput".into(), video.attachment());
            if action_id == "documentpicker.save" {
                params.insert("WFAskWhereToSave".into(), json!(true));
            }
            sc.action(action_id, params);
        }

        let mut params = Map::new();
        params.insert("GroupingIdentifier".into(), Value::String(group.clone()));
        params.insert("WFControlFlowMode".into(), json!(2));
        sc.action("choosefrommenu", params);
    }

    let question_count = sc.questions.len();
    let workflow = json!({
        "WFWorkflowClientVersion": "2607.0.2",
        "WFWorkflowMinimumClientVersion": 900,
        "WFWorkflowMinimumClientVersionString": "900",
        "WFWorkflowIcon": {
            "WFWorkflowIconStartColor": 4282601983u64,
            "WFWorkflowIconGlyphNumber": 59511,
        },
        "WFWorkflowTypes": ["ActionExtension"],
        "WFWorkflowInputContentItemClasses": ["WFURLContentItem", "WFStringContentItem"],
        "WFWorkflowNoInputBehavior": {
            "Name": "WFWorkflowNoInputBehaviorGetClipboard",
            "Parameters": {},
        },
        "WFWorkflowHasShortcutInputVariables": true,
        "WFWorkflowImportQuestions": sc.questions,
        "WFWorkflowActions": sc.actions,
    });

    let base = env::temp_dir().join(format!("ixian-shortcut-{}", std::process::id()));
    let json_path = base.with_extension("json");
    let plist_path = base.with_extension("shortcut");
    let _temp = TempFiles(vec![json_path.clone(), plist_path.clone()]);

    let bytes = serde_json::to_vec(&workflow).map_err(|error| error.to_string())?;
    fs::write(&json_path, bytes)
        .map_err(|error| format!("cannot write {}: {error}", json_path.display()))?;

    run(Command::new("plutil")
        .arg("-convert")
        .arg("binary1")
        .arg("-o")
        .arg(&plist_path)
        .arg(&json_path))?;
    run(Command::new("shortcuts")
        .arg("sign")
        .arg("--mode")
        .arg("anyone")
        .arg("--input")
        .arg(&plist_path)
        .arg("--output")
        .arg(&args.out))?;

    println!(
        "wrote {}{}",
        args.out.display(),
        if question_count > 0 {
            format!(" (asks {question_count} questions on import)")
        } else {
            String::new()
        }
    );
    Ok(())
}

fn main() {
    if let Err(error) = run_builder() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
