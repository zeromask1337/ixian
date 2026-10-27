// Builds the "ixian" Apple Shortcut (macOS only: needs `plutil` + `shortcuts sign`).
//
//   bun shortcut/build.ts                          -> shortcut/ixian.shortcut, asks server/id/secret on import
//   bun shortcut/build.ts --server URL --id ID --secret S --out x.shortcut
//                                                  -> values baked in, no import questions
//   add --test to output the video instead of showing the menu (for `shortcuts run NAME -i in.txt -o out.mp4`)
const flags = new Map<string, string | true>();
for (const [i, arg] of Bun.argv.slice(2).entries()) {
  if (!arg.startsWith("--")) continue;
  const next = Bun.argv[i + 3];
  flags.set(arg.slice(2), next && !next.startsWith("--") ? next : true);
}
const args = {
  server: flags.get("server") as string | undefined,
  id: flags.get("id") as string | undefined,
  secret: flags.get("secret") as string | undefined,
  out: (flags.get("out") as string | undefined) ?? `${import.meta.dir}/ixian.shortcut`,
  test: flags.has("test"),
};

const DEFAULT_SERVER = "https://ixian.astrapolaris.studio";
let n = 0;
const uuid = () => `1A1A0000-0000-4000-8000-${String(++n).padStart(12, "0")}`;

type Ref = { uuid: string; name: string } | { type: "ExtensionInput" };
const out = (ref: Ref) => ("type" in ref ? { Type: ref.type } : { OutputUUID: ref.uuid, OutputName: ref.name, Type: "ActionOutput" });
const shortcutInput: Ref = { type: "ExtensionInput" };
/** Text with embedded variables: tokens(ref, "/download") */
const tokens = (...parts: (string | Ref)[]) => {
  let string = "";
  const attachmentsByRange: Record<string, object> = {};
  for (const part of parts) {
    if (typeof part === "string") string += part;
    else (attachmentsByRange[`{${string.length}, 1}`] = out(part)), (string += "\uFFFC");
  }
  return { Value: { string, attachmentsByRange }, WFSerializationType: "WFTextTokenString" };
};
const attach = (ref: Ref) => ({ Value: out(ref), WFSerializationType: "WFTextTokenAttachment" });
const dict = (entries: Record<string, string | Ref>) => ({
  Value: {
    WFDictionaryFieldValueItems: Object.entries(entries).map(([key, value]) => ({
      WFItemType: 0,
      WFKey: tokens(key),
      WFValue: tokens(value),
    })),
  },
  WFSerializationType: "WFDictionaryFieldValue",
});

const actions: object[] = [];
const questions: object[] = [];
const action = (id: string, params: object = {}, name?: string): Ref => {
  const ref = { uuid: uuid(), name: name ?? "" };
  actions.push({ WFWorkflowActionIdentifier: `is.workflow.actions.${id}`, WFWorkflowActionParameters: { UUID: ref.uuid, ...params } });
  return ref;
};
const text = (value: string | undefined, question: string, fallback = "") => {
  if (value === undefined)
    questions.push({ ActionIndex: actions.length, Category: "Parameter", ParameterKey: "WFTextActionText", Text: question, DefaultValue: fallback });
  return action("gettext", { WFTextActionText: value ?? fallback, CustomOutputName: question }, question);
};
/** if/otherwise/end block; `when` gets WFCondition etc. */
const branch = (input: Ref, when: object, then: () => void, otherwise?: () => void) => {
  const group = { GroupingIdentifier: uuid() };
  action("conditional", { ...group, WFControlFlowMode: 0, WFInput: { Type: "Variable", Variable: attach(input) }, ...when });
  then();
  if (otherwise) action("conditional", { ...group, WFControlFlowMode: 1 }), otherwise();
  action("conditional", { ...group, WFControlFlowMode: 2 }, "If Result");
};
const IS = 4, HAS_NO_VALUE = 101;
const alert = (title: string, ...message: (string | Ref)[]) =>
  action("alert", { WFAlertActionTitle: title, WFAlertActionMessage: tokens(...message), WFAlertActionCancelButtonShown: false });

// --- the shortcut ---
const server = text(args.server, "ixian server URL", DEFAULT_SERVER);
const id = text(args.id, "CF-Access-Client-Id");
const secret = text(args.secret, "CF-Access-Client-Secret");

// shared input as text first: share sheets hand over URLs, text, or both
const urls = action("detect.link", { WFInput: tokens(shortcutInput) }, "URLs");
branch(urls, { WFCondition: HAS_NO_VALUE }, () => {
  alert("ixian", "No link found. Share a YouTube, X or Instagram post.");
  action("exit");
});
const link = action("getitemfromlist", { WFInput: attach(urls), WFItemSpecifier: "First Item" }, "Item from List");

const video = action(
  "downloadurl",
  {
    WFURL: tokens(server, "/download"),
    WFHTTPMethod: "POST",
    Advanced: true,
    ShowHeaders: true,
    WFHTTPHeaders: dict({ "CF-Access-Client-Id": id, "CF-Access-Client-Secret": secret }),
    WFHTTPBodyType: "JSON",
    WFJSONValues: dict({ url: link }),
  },
  "Contents of URL",
);

const ext = action("properties.files", { WFInput: attach(video), WFContentItemPropertyName: "File Extension" }, "File Extension");
branch(ext, { WFCondition: IS, WFConditionalActionString: "json" }, () => {
  const error = action("getvalueforkey", { WFInput: attach(video), WFDictionaryKey: "error" }, "Dictionary Value");
  alert("ixian failed", error);
  action("exit");
});
branch(ext, { WFCondition: IS, WFConditionalActionString: "html" }, () => {
  alert("ixian unreachable", "Got a web page instead of a video. Check the server URL and the Cloudflare Access client id/secret.");
  action("exit");
});

if (args.test) {
  action("getvariable", { WFVariable: attach(video) });
} else {
  const items = ["Save to Photos", "Share", "Save to Files"];
  const group = { GroupingIdentifier: uuid() };
  action("choosefrommenu", { ...group, WFControlFlowMode: 0, WFMenuPrompt: "ixian", WFMenuItems: items });
  const cases = {
    "Save to Photos": () => action("savetocameraroll", { WFInput: attach(video) }),
    Share: () => action("share", { WFInput: attach(video) }),
    "Save to Files": () => action("documentpicker.save", { WFInput: attach(video), WFAskWhereToSave: true }),
  };
  for (const [title, run] of Object.entries(cases)) {
    action("choosefrommenu", { ...group, WFControlFlowMode: 1, WFMenuItemTitle: title });
    run();
  }
  action("choosefrommenu", { ...group, WFControlFlowMode: 2 });
}

const workflow = {
  WFWorkflowClientVersion: "2607.0.2",
  WFWorkflowMinimumClientVersion: 900,
  WFWorkflowMinimumClientVersionString: "900",
  WFWorkflowIcon: { WFWorkflowIconStartColor: 4282601983, WFWorkflowIconGlyphNumber: 59511 },
  WFWorkflowTypes: ["ActionExtension"],
  WFWorkflowInputContentItemClasses: ["WFURLContentItem", "WFStringContentItem"],
  WFWorkflowNoInputBehavior: { Name: "WFWorkflowNoInputBehaviorGetClipboard", Parameters: {} },
  WFWorkflowHasShortcutInputVariables: true,
  WFWorkflowImportQuestions: questions,
  WFWorkflowActions: actions,
};

const tmp = `${Bun.env.TMPDIR ?? "/tmp"}/ixian-shortcut-${process.pid}`;
await Bun.write(`${tmp}.json`, JSON.stringify(workflow));
await Bun.$`plutil -convert binary1 -o ${tmp}.shortcut ${tmp}.json`;
await Bun.$`shortcuts sign --mode anyone --input ${tmp}.shortcut --output ${args.out}`.quiet();
await Promise.all([Bun.file(`${tmp}.json`).delete(), Bun.file(`${tmp}.shortcut`).delete()]);
console.log(`wrote ${args.out}${questions.length ? ` (asks ${questions.length} questions on import)` : ""}`);
