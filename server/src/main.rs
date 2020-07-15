use rust_lsp::jsonrpc::method_types::*;
use rust_lsp::jsonrpc::*;
use rust_lsp::lsp::*;
use rust_lsp::lsp_types::*;
use rust_lsp::lsp_types::notification::*;

use walkdir;

use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::ops::Add;
use std::process;
use std::rc::Rc;
use std::num::NonZeroU64;
use std::str::FromStr;

use chan::WaitGroup;

use percent_encoding;

use regex::Regex;

use lazy_static::lazy_static;

mod graph;
mod provider;
#[cfg(test)]
mod test;

lazy_static! {
    static ref RE_DIAGNOSTIC: Regex = Regex::new(r#"^(ERROR|WARNING): ([^?<>*|"]+?):(\d+): (?:'.*?' : )?(.+)\r?"#).unwrap();
    static ref RE_VERSION: Regex = Regex::new(r#"#version [\d]{3}"#).unwrap();
    static ref RE_INCLUDE: Regex = Regex::new(r#"^(?:\s)*?(?:#include) "(.+)"\r?"#).unwrap();
    static ref RE_INCLUDE_EXTENSION: Regex = Regex::new(r#"#extension GL_GOOGLE_include_directive ?: ?require"#).unwrap();
}

#[allow(dead_code)]
static INCLUDE_STR: &'static str = "#extension GL_GOOGLE_include_directive : require";

static SOURCE: &'static str = "mc-glsl";

fn main() {
    let stdin = std::io::stdin();

    let endpoint_output = LSPEndpoint::create_lsp_output_with_output_stream(|| io::stdout());

    let cache_graph = graph::CachedStableGraph::new();

    let mut langserver = MinecraftShaderLanguageServer {
        endpoint: endpoint_output.clone(),
        graph: Rc::new(RefCell::new(cache_graph)),
        config: Configuration::default(),
        wait: WaitGroup::new(),
        root: None,
        command_provider: None,
    };

    langserver.command_provider = Some(provider::CustomCommandProvider::new(vec![(
        "graphDot",
        Box::new(provider::GraphDotCommand {
            graph: langserver.graph.clone(),
        }),
    )]));

    LSPEndpoint::run_server_from_input(&mut stdin.lock(), endpoint_output, langserver);
}

struct MinecraftShaderLanguageServer {
    endpoint: Endpoint,
    graph: Rc<RefCell<graph::CachedStableGraph>>,
    config: Configuration,
    wait: WaitGroup,
    root: Option<String>,
    command_provider: Option<provider::CustomCommandProvider>,
}

struct Configuration {
    glslang_validator_path: String,
    shaderpacks_path: String,
}

impl Default for Configuration {
    fn default() -> Self {
        let shaderpacks_path = std::env::var("HOME").unwrap() + "/.minecraft/shaderpacks";
        Configuration{
            glslang_validator_path: String::from("glslangValidator"),
            shaderpacks_path
        }
    }
}

impl Configuration {
    fn validate(&self) -> bool {
        if self.glslang_validator_path == "" || self.shaderpacks_path == "" {
            return false;
        }

        return true;
    }
}

struct GLSLFile {
    idx: petgraph::graph::NodeIndex,
    includes: Vec<IncludePosition>,
}

#[derive(Clone)]
pub struct IncludePosition {
    filepath: String,
    line: u64,
    start: u64,
    end: u64,
}

impl Display for IncludePosition {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{{{}, l {}, s, {}, e {}}}", self.filepath, self.line, self.start, self.end)
    }
}

impl MinecraftShaderLanguageServer {
    pub fn error_not_available<DATA>(data: DATA) -> MethodError<DATA> {
        let msg = "Functionality not implemented.".to_string();
        MethodError::<DATA> {
            code: 1,
            message: msg,
            data,
        }
    }

    pub fn gen_initial_graph(&self, root: &str) {
        self.endpoint
            .send_notification("status", vec!["$(loading~spin)", "Building project..."])
            .unwrap();
        let mut files = HashMap::new();

        eprintln!("root of project is {}", root);

        // filter directories and files not ending in any of the 3 extensions
        let file_iter = walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|entry| {
                if !entry.is_ok() {
                    return None;
                }

                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    return None;
                }

                let ext = match path.extension() {
                    Some(e) => e,
                    None => {
                        eprintln!("filepath {} had no extension", path.to_str().unwrap());
                        return None;
                    }
                };
                if ext != "vsh" && ext != "fsh" && ext != "glsl" && ext != "inc" {
                    return None;
                }
                Some(String::from(path.to_str().unwrap()))
            });

        // iterate all valid found files, search for includes, add a node into the graph for each
        // file and add a file->includes KV into the map
        for path in file_iter {
            let includes = self.find_includes(root, path.as_str());

            let idx = self.graph.borrow_mut().add_node(path.clone());

            //eprintln!("adding {} with\n{:?}", path.clone(), includes);

            files.insert(path, GLSLFile { idx, includes });
        }

        // Add edges between nodes, finding target nodes on weight (value)
        for (_, v) in files.into_iter() {
            for file in v.includes {
                //eprintln!("searching for {}", file);
                let idx = self.graph.borrow_mut().find_node(file.filepath.as_str());
                if idx.is_none() {
                    eprintln!("couldn't find {} in graph for {}", file, self.graph.borrow().graph[v.idx]);
                    continue;
                }
                //eprintln!("added edge between\n\t{}\n\t{}", k, file);
                self.graph.borrow_mut().add_edge(
                    v.idx,
                    idx.unwrap(),
                    file.line,
                    file.start,
                    file.end,
                );
                //self.graph.borrow_mut().graph.add_edge(v.idx, idx.unwrap(), String::from("includes"));
            }
        }

        eprintln!("finished building project include graph");
        std::thread::sleep(std::time::Duration::from_secs(1));
        self.endpoint
            .send_notification("status", vec!["$(check)", "Finished building project!"])
            .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        self.endpoint
            .send_notification("clearStatus", None::<()>)
            .unwrap();
    }

    pub fn find_includes(&self, root: &str, file: &str) -> Vec<IncludePosition> {
        let mut includes = Vec::default();

        let buf = BufReader::new(std::fs::File::open(file).unwrap());
        buf.lines()
            .enumerate()
            .filter_map(|line| match line.1 {
                Ok(t) => Some((line.0, t)),
                Err(_e) => None,
            })
            .filter(|line| RE_INCLUDE.is_match(line.1.as_str()))
            .for_each(|line| {
                let cap = RE_INCLUDE
                    .captures(line.1.as_str())
                    .unwrap()
                    .get(1)
                    .unwrap();
                //eprintln!("{:?}", caps);

                let start = u64::try_from(cap.start()).unwrap();
                let end = u64::try_from(cap.end()).unwrap();
                let mut path: String = String::from(cap.as_str());
                if !path.starts_with("/") {
                    path.insert(0, '/');
                }
                let full_include = String::from(root).add("/shaders").add(path.as_str());
                includes.push(IncludePosition {
                    filepath: full_include.clone(),
                    line: u64::try_from(line.0).unwrap(),
                    start,
                    end,
                });
                //eprintln!("{} includes {}", file, full_include);
            });

        return includes;
    }

    pub fn lint(&self, source: impl Into<String>) -> Vec<Diagnostic> {
        let source: String = source.into();
        eprintln!("validator bin path: {}", self.config.glslang_validator_path);
        let cmd = process::Command::new(&self.config.glslang_validator_path)
            .args(&["--stdin", "-S", "frag"])
            .stdin(process::Stdio::piped())
            .stdout(process::Stdio::piped())
            .spawn();

        let mut child = cmd.expect("glslangValidator failed to spawn");
        let stdin = child.stdin.as_mut().expect("no stdin handle found");
        stdin.write(source.as_bytes()).expect("failed to write to stdin");
        
        let output = child.wait_with_output().expect("expected output");
        let stdout = String::from_utf8(output.stdout).unwrap();
        eprintln!("glslangValidator output: {}\n", stdout);

        let mut diagnostics: Vec<Diagnostic> = vec![];

        let source_lines: Vec<&str> = source.split('\n').collect();

        stdout.split('\n').into_iter().for_each(|line| {
            let diagnostic_capture = match RE_DIAGNOSTIC.captures(line) {
                Some(d) => d,
                None => return
            };

            eprintln!("match {:?}", diagnostic_capture);
            
            let msg = diagnostic_capture.get(4).unwrap().as_str().trim();

            if msg.starts_with("compilation terminated") {
                return
            }

            let line = NonZeroU64::from_str(
                diagnostic_capture
                    .get(3)
                    .expect("third capture group was None")
                    .as_str()
            ).expect("got non-numeric string value");
            let line: u64 = line.into();
            let line = line - 1;
            

            let line_text = source_lines[line as usize];
            let leading_whitespace = line_text.len() - line_text.trim_start().len();

            let severity = match diagnostic_capture.get(0).unwrap().as_str() {
                "ERROR" => DiagnosticSeverity::Error,
                "WARNING" => DiagnosticSeverity::Warning,
                _ => DiagnosticSeverity::Information,
            };


            let diagnostic = Diagnostic {
                range: Range::new(
                    Position::new(line, leading_whitespace as u64),
                    Position::new(line, line_text.len() as u64)
                ),
                code: None,
                severity: Some(severity),
                source: Some(String::from(SOURCE)),
                message: String::from(msg),
                related_information: None,
                tags: None,
            };

            diagnostics.push(diagnostic);
        });
        diagnostics
    }

    pub fn publish_diagnostic(&self, diagnostics: Vec<Diagnostic>, uri: impl Into<Url>, document_version: Option<i64>) {
        self.endpoint.send_notification(PublishDiagnostics::METHOD, PublishDiagnosticsParams {
            uri: uri.into(),
            diagnostics,
            version: document_version,
        }).unwrap();
    }
}

impl LanguageServerHandling for MinecraftShaderLanguageServer {
    fn initialize(&mut self, params: InitializeParams, completable: MethodCompletable<InitializeResult, InitializeError>) {
        self.wait.add(1);

        let mut capabilities = ServerCapabilities::default();
        capabilities.hover_provider = Some(false);
        capabilities.document_link_provider = Some(DocumentLinkOptions {
            resolve_provider: None,
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: None,
            },
        });
        capabilities.execute_command_provider = Some(ExecuteCommandOptions {
            commands: vec![String::from("graphDot")],
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: None,
            },
        });
        capabilities.text_document_sync = Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                will_save: None,
                will_save_wait_until: None,
                change: Some(TextDocumentSyncKind::Full),
                save: Some(SaveOptions {
                    include_text: Some(true),
                }),
            },
        ));

        self.root = Some(percent_encoding::percent_decode_str(params.root_uri.unwrap().path()).decode_utf8().unwrap().into());

        self.gen_initial_graph(self.root.as_ref().unwrap());

        completable.complete(Ok(InitializeResult {
            capabilities,
            server_info: None,
        }));
    }

    fn shutdown(&mut self, _: (), completable: LSCompletable<()>) {
        completable.complete(Ok(()));
    }

    fn exit(&mut self, _: ()) {
        self.endpoint.request_shutdown();
    }

    fn workspace_change_configuration(&mut self, params: DidChangeConfigurationParams) {
        let config = params.settings.as_object().unwrap().get("mcglsl").unwrap();

        self.config.glslang_validator_path = String::from(
            config
                .get("glslangValidatorPath")
                .unwrap()
                .as_str()
                .unwrap(),
        );
        self.config.shaderpacks_path =
            String::from(config.get("shaderpacksPath").unwrap().as_str().unwrap());

        if !self.config.validate() {
            self.endpoint
                .send_notification("badConfig", None::<()>)
                .unwrap();
        }

        eprintln!("{:?}", params.settings.as_object().unwrap());

        self.wait.done();
    }

    fn did_open_text_document(&mut self, params: DidOpenTextDocumentParams) {
        eprintln!("opened doc {}", params.text_document.uri);
        let diagnostics = self.lint(params.text_document.text);
        self.publish_diagnostic(diagnostics, params.text_document.uri, Some(params.text_document.version));
    }

    fn did_change_text_document(&mut self, params: DidChangeTextDocumentParams) {
        self.wait.wait();

        #[allow(unused_variables)]
        let text_change = params.content_changes[0].clone();
        //eprintln!("changed {} changes: {}", text_change., params.text_document.uri);
    }

    fn did_close_text_document(&mut self, _: DidCloseTextDocumentParams) {}

    fn did_save_text_document(&mut self, params: DidSaveTextDocumentParams) {
        self.wait.wait();

        let path: String = percent_encoding::percent_decode_str(params.text_document.uri.path()).decode_utf8().unwrap().into();
        
        let file_content = std::fs::read(path).unwrap();
        let diagnostics = self.lint(String::from_utf8(file_content).unwrap());
        self.publish_diagnostic(diagnostics, params.text_document.uri, None);
    }

    fn did_change_watched_files(&mut self, _: DidChangeWatchedFilesParams) {}

    fn completion(&mut self, _: TextDocumentPositionParams, completable: LSCompletable<CompletionList>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn resolve_completion_item(&mut self, _: CompletionItem, completable: LSCompletable<CompletionItem>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn hover(&mut self, _: TextDocumentPositionParams, _: LSCompletable<Hover>) {
        self.wait.wait();
        /* completable.complete(Ok(Hover{
            contents: HoverContents::Markup(MarkupContent{
                kind: MarkupKind::Markdown,
                value: String::from("# Hello World"),
            }),
            range: None,
        })); */
    }

    fn execute_command(&mut self, mut params: ExecuteCommandParams, completable: LSCompletable<WorkspaceEdit>) {
        params
            .arguments
            .push(serde_json::Value::String(self.root.clone().unwrap()));
        match self
            .command_provider
            .as_ref()
            .unwrap()
            .execute(params.command.as_ref(), params.arguments)
        {
            Ok(_) => {
                eprintln!("executed {} successfully", params.command);
                // TODO: notification popup
                completable.complete(Ok(WorkspaceEdit {
                    changes: None,
                    document_changes: None,
                }))
            },
            Err(err) => {
                eprintln!("failed to execute {}: {}", params.command, err);
                completable.complete(Err(MethodError::new(32420, err, ())))
            },
        }
    }

    fn signature_help(&mut self, _: TextDocumentPositionParams, completable: LSCompletable<SignatureHelp>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn goto_definition(&mut self, _: TextDocumentPositionParams, completable: LSCompletable<Vec<Location>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn references(&mut self, _: ReferenceParams, completable: LSCompletable<Vec<Location>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn document_highlight(&mut self, _: TextDocumentPositionParams, completable: LSCompletable<Vec<DocumentHighlight>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn document_symbols(&mut self, _: DocumentSymbolParams, completable: LSCompletable<Vec<SymbolInformation>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn workspace_symbols(&mut self, _: WorkspaceSymbolParams, completable: LSCompletable<Vec<SymbolInformation>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn code_action(&mut self, _: CodeActionParams, completable: LSCompletable<Vec<Command>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn code_lens(&mut self, _: CodeLensParams, completable: LSCompletable<Vec<CodeLens>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn code_lens_resolve(&mut self, _: CodeLens, completable: LSCompletable<CodeLens>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn document_link(&mut self, params: DocumentLinkParams, completable: LSCompletable<Vec<DocumentLink>>) {
        eprintln!("document link file: {:?}", params.text_document.uri.to_file_path().unwrap());
        // node for current document
        let curr_doc = params
            .text_document
            .uri
            .to_file_path()
            .unwrap()
            .as_os_str()
            .to_str()
            .unwrap()
            .to_string();
        let node = match self.graph.borrow_mut().find_node(curr_doc) {
            Some(n) => n,
            None => {
                completable.complete(Ok(vec![]));
                return
            },
        };

        let edges: Vec<DocumentLink> = self
            .graph
            .borrow()
            .get_include_meta(node)
            .into_iter()
            .filter_map(|value| {
                let path = std::path::Path::new(&value.filepath);
                let url = match Url::from_file_path(path) {
                    Ok(url) => url,
                    Err(e) => {
                        eprintln!("error converting {:?} into url: {:?}", path, e);
                        return None;
                    }
                };

                Some(DocumentLink {
                    range: Range::new(
                        Position::new(value.line, value.start),
                        Position::new(value.line, value.end),
                    ),
                    target: Some(url.clone()),
                    //tooltip: Some(url.path().to_string().strip_prefix(self.root.clone().unwrap().as_str()).unwrap().to_string()),
                    tooltip: None,
                    data: None,
                })
            })
            .collect();
        eprintln!("links: {:?}", edges);
        completable.complete(Ok(edges));
    }

    fn document_link_resolve(&mut self, _: DocumentLink, completable: LSCompletable<DocumentLink>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn formatting(&mut self, _: DocumentFormattingParams, completable: LSCompletable<Vec<TextEdit>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn range_formatting(&mut self, _: DocumentRangeFormattingParams, completable: LSCompletable<Vec<TextEdit>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn on_type_formatting(&mut self, _: DocumentOnTypeFormattingParams, completable: LSCompletable<Vec<TextEdit>>) {
        completable.complete(Err(Self::error_not_available(())));
    }

    fn rename(&mut self, _: RenameParams, completable: LSCompletable<WorkspaceEdit>) {
        completable.complete(Err(Self::error_not_available(())));
    }
}
