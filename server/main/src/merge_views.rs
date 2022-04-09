use std::cmp::min;
use std::iter::Peekable;
use std::{
    collections::{HashMap, LinkedList, VecDeque},
    path::{Path, PathBuf},
};

use core::slice::Iter;

use petgraph::stable_graph::NodeIndex;

use crate::graph::CachedStableGraph;
use crate::source_mapper::SourceMapper;

/// FilialTuple represents a tuple with a child at index 0
/// and a parent at index 1. Parent can be nullable in the case of
/// the child being a top level node in the tree.
pub type FilialTuple = (NodeIndex, Option<NodeIndex>);

pub fn generate_merge_list<'a>(
    nodes: &'a [FilialTuple], sources: &'a HashMap<PathBuf, String>, graph: &'a CachedStableGraph, source_mapper: &mut SourceMapper,
) -> String {
    // contains additionally inserted lines such as #line and other directives, preamble defines etc
    let mut extra_lines: Vec<String> = Vec::new();
    extra_lines.reserve((nodes.len() * 2) + 2);

    // list of source code views onto the below sources
    let mut merge_list: LinkedList<&'a str> = LinkedList::new();

    // holds the offset into the child which has been added to the merge list for a parent.
    // A child can have multiple parents for a given tree, hence we have to track it for
    // a (child, parent) tuple instead of just the child.
    let mut last_offset_set: HashMap<FilialTuple, usize> = HashMap::new();

    let mut nodes_iter = nodes.iter().peekable();

    // invariant: nodes_iter always has _at least_ one element. Can't save a not-file :B
    let first = nodes_iter.next().unwrap().0;
    let first_path = graph.get_node(first);
    let first_source = sources.get(&first_path).unwrap();

    // seed source_mapper with top-level file
    source_mapper.get_num(first);

    let version_line_offset = find_version_offset(first_source);
    let version_char_offsets = char_offset_for_line(version_line_offset, first_source);
    // add_preamble(
    //     version_line_offset,
    //     version_char_offsets.1,
    //     &first_path,
    //     first,
    //     first_source,
    //     &mut merge_list,
    //     &mut extra_lines,
    //     source_mapper,
    // );

    // last_offset_set.insert((first, None), version_char_offsets.1);
    last_offset_set.insert((first, None), 0);

    // stack to keep track of the depth first traversal
    let mut stack = VecDeque::<NodeIndex>::new();

    create_merge_views(
        &mut nodes_iter,
        &mut merge_list,
        &mut last_offset_set,
        graph,
        sources,
        &mut extra_lines,
        &mut stack,
        source_mapper,
    );

    // now we add a view of the remainder of the root file
    let offset = *last_offset_set.get(&(first, None)).unwrap();

    let len = first_source.len();
    merge_list.push_back(&first_source[min(offset, len)..]);

    let total_len = merge_list.iter().fold(0, |a, b| a + b.len());

    let mut merged = String::with_capacity(total_len);
    merged.extend(merge_list);

    merged
}

fn create_merge_views<'a>(
    nodes: &mut Peekable<Iter<FilialTuple>>, merge_list: &mut LinkedList<&'a str>, last_offset_set: &mut HashMap<FilialTuple, usize>,
    graph: &'a CachedStableGraph, sources: &'a HashMap<PathBuf, String>, extra_lines: &mut Vec<String>, stack: &mut VecDeque<NodeIndex>,
    source_mapper: &mut SourceMapper,
) {
    loop {
        let n = match nodes.next() {
            Some(n) => n,
            None => return,
        };

        // invariant: never None as only the first element in `nodes` should have a None, which is popped off in the calling function
        let parent = n.1.unwrap();
        let child = n.0;
        let edge = graph.get_edge_meta(parent, child);
        let parent_path = graph.get_node(parent).clone();
        let child_path = graph.get_node(child).clone();

        let parent_source = sources.get(&parent_path).unwrap();
        let (char_for_line, char_following_line) = char_offset_for_line(edge.line, parent_source);

        let offset = *last_offset_set
            .insert((parent, stack.back().copied()), char_following_line)
            .get_or_insert(0);
        merge_list.push_back(&parent_source[offset..char_for_line]);
        add_opening_line_directive(&child_path, child, merge_list, extra_lines, source_mapper);

        match nodes.peek() {
            Some(next) => {
                let next = *next;
                // if the next pair's parent is not a child of the current pair, we dump the rest of this childs source
                if next.1.unwrap() != child {
                    let child_source = sources.get(&child_path).unwrap();
                    // if ends in \n\n, we want to exclude the last \n for some reason. Ask optilad
                    let offset = {
                        match child_source.ends_with('\n') {
                            true => child_source.len() - 1,
                            false => child_source.len(),
                        }
                    };
                    merge_list.push_back(&child_source[..offset]);
                    last_offset_set.insert((child, Some(parent)), 0);
                    // +2 because edge.line is 0 indexed but #line is 1 indexed and references the *following* line
                    add_closing_line_directive(edge.line + 2, &parent_path, parent, merge_list, extra_lines, source_mapper);
                    // if the next pair's parent is not the current pair's parent, we need to bubble up
                    if stack.contains(&next.1.unwrap()) {
                        return;
                    }
                    continue;
                }

                stack.push_back(parent);
                create_merge_views(
                    nodes,
                    merge_list,
                    last_offset_set,
                    graph,
                    sources,
                    extra_lines,
                    stack,
                    source_mapper,
                );
                stack.pop_back();

                let offset = *last_offset_set.get(&(child, Some(parent))).unwrap();
                let child_source = sources.get(&child_path).unwrap();
                // this evaluates to false once the file contents have been exhausted aka offset = child_source.len() + 1
                let end_offset = match child_source.ends_with('\n') {
                    true => 1,  /* child_source.len()-1 */
                    false => 0, /* child_source.len() */
                };
                if offset < child_source.len() - end_offset {
                    // if ends in \n\n, we want to exclude the last \n for some reason. Ask optilad
                    merge_list.push_back(&child_source[offset../* std::cmp::max( */child_source.len()-end_offset/* , offset) */]);
                    last_offset_set.insert((child, Some(parent)), 0);
                }

                // +2 because edge.line is 0 indexed but #line is 1 indexed and references the *following* line
                add_closing_line_directive(edge.line + 2, &parent_path, parent, merge_list, extra_lines, source_mapper);

                // we need to check the next item at the point of original return further down the callstack
                if nodes.peek().is_some() && stack.contains(&nodes.peek().unwrap().1.unwrap()) {
                    return;
                }
            }
            None => {
                let child_source = sources.get(&child_path).unwrap();
                // if ends in \n\n, we want to exclude the last \n for some reason. Ask optilad
                let offset = match child_source.ends_with('\n') {
                    true => child_source.len() - 1,
                    false => child_source.len(),
                };
                merge_list.push_back(&child_source[..offset]);
                last_offset_set.insert((child, Some(parent)), 0);
                // +2 because edge.line is 0 indexed but #line is 1 indexed and references the *following* line
                add_closing_line_directive(edge.line + 2, &parent_path, parent, merge_list, extra_lines, source_mapper);
            }
        }
    }
}

// returns the character offset + 1 of the end of line number `line` and the character
// offset + 1 for the end of the line after the previous one
fn char_offset_for_line(line_num: usize, source: &str) -> (usize, usize) {
    let mut char_for_line: usize = 0;
    let mut char_following_line: usize = 0;
    for (n, line) in source.lines().enumerate() {
        if n == line_num {
            char_following_line += line.len() + 1;
            break;
        }
        char_for_line += line.len() + 1;
        char_following_line = char_for_line;
    }
    (char_for_line, char_following_line)
}

fn find_version_offset(source: &str) -> usize {
    source
        .lines()
        .enumerate()
        .find(|(_, line)| line.starts_with("#version "))
        .map_or(0, |(i, _)| i)
}

// fn add_preamble<'a>(
//     version_line_offset: usize, version_char_offset: usize, path: &Path, node: NodeIndex, source: &'a str,
//     merge_list: &mut LinkedList<&'a str>, extra_lines: &mut Vec<String>, source_mapper: &mut SourceMapper,
// ) {
//     // TODO: Optifine #define preabmle
//     merge_list.push_back(&source[..version_char_offset]);
//     let google_line_directive = format!(
//         "#extension GL_GOOGLE_cpp_style_line_directive : enable\n#line {} {} // {}\n",
//         // +2 because 0 indexed but #line is 1 indexed and references the *following* line
//         version_line_offset + 2,
//         source_mapper.get_num(node),
//         path.to_str().unwrap().replace('\\', "\\\\"),
//     );
//     extra_lines.push(google_line_directive);
//     unsafe_get_and_insert(merge_list, extra_lines);
// }

fn add_opening_line_directive(
    path: &Path, node: NodeIndex, merge_list: &mut LinkedList<&str>, extra_lines: &mut Vec<String>, source_mapper: &mut SourceMapper,
) {
    let line_directive = format!(
        "#line 1 {} // {}\n",
        source_mapper.get_num(node),
        path.to_str().unwrap().replace('\\', "\\\\")
    );
    extra_lines.push(line_directive);
    unsafe_get_and_insert(merge_list, extra_lines);
}

fn add_closing_line_directive(
    line: usize, path: &Path, node: NodeIndex, merge_list: &mut LinkedList<&str>, extra_lines: &mut Vec<String>,
    source_mapper: &mut SourceMapper,
) {
    // Optifine doesn't seem to add a leading newline if the previous line was a #line directive
    let line_directive = if let Some(l) = merge_list.back() {
        if l.trim().starts_with("#line") {
            format!(
                "#line {} {} // {}\n",
                line,
                source_mapper.get_num(node),
                path.to_str().unwrap().replace('\\', "\\\\")
            )
        } else {
            format!(
                "\n#line {} {} // {}\n",
                line,
                source_mapper.get_num(node),
                path.to_str().unwrap().replace('\\', "\\\\")
            )
        }
    } else {
        format!(
            "\n#line {} {} // {}\n",
            line,
            source_mapper.get_num(node),
            path.to_str().unwrap().replace('\\', "\\\\")
        )
    };

    extra_lines.push(line_directive);
    unsafe_get_and_insert(merge_list, extra_lines);
}

fn unsafe_get_and_insert(merge_list: &mut LinkedList<&str>, extra_lines: &[String]) {
    // :^)
    unsafe {
        let vec_ptr_offset = extra_lines.as_ptr().add(extra_lines.len() - 1);
        merge_list.push_back(&vec_ptr_offset.as_ref().unwrap()[..]);
    }
}

#[cfg(test)]
mod merge_view_test {
    use std::fs;
    use std::path::PathBuf;

    use crate::merge_views::generate_merge_list;
    use crate::source_mapper::SourceMapper;
    use crate::test::{copy_to_and_set_root, new_temp_server};
    use crate::IncludePosition;

    #[test]
    #[logging_macro::log_scope]
    fn test_generate_merge_list_01() {
        let mut server = new_temp_server(None);

        let (_tmp_dir, tmp_path) = copy_to_and_set_root("./testdata/01", &mut server);
        server.endpoint.request_shutdown();

        let final_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{:?}/shaders/final.fsh", tmp_path).try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("final.fsh"));
        let common_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{:?}/shaders/common.glsl", tmp_path).try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("common.glsl"));

        server
            .graph
            .borrow_mut()
            .add_edge(final_idx, common_idx, IncludePosition { line: 2, start: 0, end: 0 });

        let nodes = server.get_dfs_for_node(final_idx).unwrap();
        let sources = server.load_sources(&nodes).unwrap();

        let graph_borrow = server.graph.borrow();
        let mut source_mapper = SourceMapper::new(0);
        let result = generate_merge_list(&nodes, &sources, &graph_borrow, &mut source_mapper);

        let merge_file = tmp_path.join("shaders").join("final.fsh.merge");

        let mut truth = fs::read_to_string(merge_file).unwrap();
        // truth = truth.replacen(
        //     "!!",
        //     &tmp_path.join("shaders").join("final.fsh").to_str().unwrap().replace('\\', "\\\\"),
        //     1,
        // );
        truth = truth.replacen(
            "!!",
            &tmp_path.join("shaders").join("common.glsl").to_str().unwrap().replace('\\', "\\\\"),
            1,
        );
        truth = truth.replace(
            "!!",
            &tmp_path.join("shaders").join("final.fsh").to_str().unwrap().replace('\\', "\\\\"),
        );

        assert_eq!(result, truth);
    }

    #[test]
    #[logging_macro::log_scope]
    fn test_generate_merge_list_02() {
        let mut server = new_temp_server(None);

        let (_tmp_dir, tmp_path) = copy_to_and_set_root("./testdata/02", &mut server);
        server.endpoint.request_shutdown();

        let final_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/{}", tmp_path, "final.fsh").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("final.fsh"));
        let test_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/utils/{}", tmp_path, "test.glsl").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("utils").join("test.glsl"));
        let burger_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/utils/{}", tmp_path, "burger.glsl").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("utils").join("burger.glsl"));
        let sample_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/utils/{}", tmp_path, "sample.glsl").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("utils").join("sample.glsl"));

        server
            .graph
            .borrow_mut()
            .add_edge(final_idx, sample_idx, IncludePosition { line: 2, start: 0, end: 0 });
        server
            .graph
            .borrow_mut()
            .add_edge(sample_idx, burger_idx, IncludePosition { line: 4, start: 0, end: 0 });
        server
            .graph
            .borrow_mut()
            .add_edge(sample_idx, test_idx, IncludePosition { line: 6, start: 0, end: 0 });

        let nodes = server.get_dfs_for_node(final_idx).unwrap();
        let sources = server.load_sources(&nodes).unwrap();

        let graph_borrow = server.graph.borrow();
        let mut source_mapper = SourceMapper::new(0);
        let result = generate_merge_list(&nodes, &sources, &graph_borrow, &mut source_mapper);

        let merge_file = tmp_path.join("shaders").join("final.fsh.merge");

        let mut truth = fs::read_to_string(merge_file).unwrap();

        // truth = truth.replacen(
        //     "!!",
        //     &tmp_path.join("shaders").join("final.fsh").to_str().unwrap().replace('\\', "\\\\"),
        //     1,
        // );

        for file in &["sample.glsl", "burger.glsl", "sample.glsl", "test.glsl", "sample.glsl"] {
            let path = tmp_path.clone();
            truth = truth.replacen(
                "!!",
                &path
                    .join("shaders")
                    .join("utils")
                    .join(file)
                    .to_str()
                    .unwrap()
                    .replace('\\', "\\\\"),
                1,
            );
        }
        truth = truth.replacen(
            "!!",
            &tmp_path.join("shaders").join("final.fsh").to_str().unwrap().replace('\\', "\\\\"),
            1,
        );

        assert_eq!(result, truth);
    }

    #[test]
    #[logging_macro::log_scope]
    fn test_generate_merge_list_03() {
        let mut server = new_temp_server(None);

        let (_tmp_dir, tmp_path) = copy_to_and_set_root("./testdata/03", &mut server);
        server.endpoint.request_shutdown();

        let final_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/{}", tmp_path, "final.fsh").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("final.fsh"));
        let test_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/utils/{}", tmp_path, "test.glsl").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("utils").join("test.glsl"));
        let burger_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/utils/{}", tmp_path, "burger.glsl").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("utils").join("burger.glsl"));
        let sample_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/utils/{}", tmp_path, "sample.glsl").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("utils").join("sample.glsl"));

        server
            .graph
            .borrow_mut()
            .add_edge(final_idx, sample_idx, IncludePosition { line: 2, start: 0, end: 0 });
        server
            .graph
            .borrow_mut()
            .add_edge(sample_idx, burger_idx, IncludePosition { line: 4, start: 0, end: 0 });
        server
            .graph
            .borrow_mut()
            .add_edge(sample_idx, test_idx, IncludePosition { line: 6, start: 0, end: 0 });

        let nodes = server.get_dfs_for_node(final_idx).unwrap();
        let sources = server.load_sources(&nodes).unwrap();

        let graph_borrow = server.graph.borrow();
        let mut source_mapper = SourceMapper::new(0);
        let result = generate_merge_list(&nodes, &sources, &graph_borrow, &mut source_mapper);

        let merge_file = tmp_path.join("shaders").join("final.fsh.merge");

        let mut truth = fs::read_to_string(merge_file).unwrap();

        // truth = truth.replacen(
        //     "!!",
        //     &tmp_path.join("shaders").join("final.fsh").to_str().unwrap().replace('\\', "\\\\"),
        //     1,
        // );

        for file in &["sample.glsl", "burger.glsl", "sample.glsl", "test.glsl", "sample.glsl"] {
            let path = tmp_path.clone();
            truth = truth.replacen(
                "!!",
                &path
                    .join("shaders")
                    .join("utils")
                    .join(file)
                    .to_str()
                    .unwrap()
                    .replace('\\', "\\\\"),
                1,
            );
        }
        truth = truth.replacen(
            "!!",
            &tmp_path.join("shaders").join("final.fsh").to_str().unwrap().replace('\\', "\\\\"),
            1,
        );

        assert_eq!(result, truth);
    }

    #[test]
    #[logging_macro::log_scope]
    fn test_generate_merge_list_04() {
        let mut server = new_temp_server(None);

        let (_tmp_dir, tmp_path) = copy_to_and_set_root("./testdata/04", &mut server);
        server.endpoint.request_shutdown();

        let final_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/{}", tmp_path, "final.fsh").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("final.fsh"));
        let utilities_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/utils/{}", tmp_path, "utilities.glsl").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("utils").join("utilities.glsl"));
        let stuff1_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/utils/{}", tmp_path, "stuff1.glsl").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("utils").join("stuff1.glsl"));
        let stuff2_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/utils/{}", tmp_path, "stuff2.glsl").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("utils").join("stuff2.glsl"));
        let matrices_idx = server
            .graph
            .borrow_mut()
            //.add_node(&format!("{}/shaders/lib/{}", tmp_path, "matrices.glsl").try_into().unwrap());
            .add_node(&tmp_path.join("shaders").join("lib").join("matrices.glsl"));

        server
            .graph
            .borrow_mut()
            .add_edge(final_idx, utilities_idx, IncludePosition { line: 2, start: 0, end: 0 });
        server
            .graph
            .borrow_mut()
            .add_edge(utilities_idx, stuff1_idx, IncludePosition { line: 0, start: 0, end: 0 });
        server
            .graph
            .borrow_mut()
            .add_edge(utilities_idx, stuff2_idx, IncludePosition { line: 1, start: 0, end: 0 });
        server
            .graph
            .borrow_mut()
            .add_edge(final_idx, matrices_idx, IncludePosition { line: 3, start: 0, end: 0 });

        let nodes = server.get_dfs_for_node(final_idx).unwrap();
        let sources = server.load_sources(&nodes).unwrap();

        let graph_borrow = server.graph.borrow();
        let mut source_mapper = SourceMapper::new(0);
        let result = generate_merge_list(&nodes, &sources, &graph_borrow, &mut source_mapper);

        let merge_file = tmp_path.join("shaders").join("final.fsh.merge");

        let mut truth = fs::read_to_string(merge_file).unwrap();

        for file in &[
            // PathBuf::new().join("final.fsh").to_str().unwrap(),
            PathBuf::new().join("utils").join("utilities.glsl").to_str().unwrap(),
            PathBuf::new().join("utils").join("stuff1.glsl").to_str().unwrap(),
            PathBuf::new().join("utils").join("utilities.glsl").to_str().unwrap(),
            PathBuf::new().join("utils").join("stuff2.glsl").to_str().unwrap(),
            PathBuf::new().join("utils").join("utilities.glsl").to_str().unwrap(),
            PathBuf::new().join("final.fsh").to_str().unwrap(),
            PathBuf::new().join("lib").join("matrices.glsl").to_str().unwrap(),
            PathBuf::new().join("final.fsh").to_str().unwrap(),
        ] {
            let path = tmp_path.clone();
            truth = truth.replacen("!!", &path.join("shaders").join(file).to_str().unwrap().replace('\\', "\\\\"), 1);
        }

        assert_eq!(result, truth);
    }
}