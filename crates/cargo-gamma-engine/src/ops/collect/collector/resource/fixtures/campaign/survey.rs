// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

struct Frame {
    node: usize,
    edge_at: usize,
}

fn strongly_connected_components(edges: &[Vec<usize>]) -> Vec<usize> {
    let node_count = edges.len();
    let mut index_of: Vec<Option<u32>> = vec![None; node_count];
    let mut low_link: Vec<u32> = vec![0; node_count];
    let mut on_stack: Vec<bool> = vec![false; node_count];
    let mut comp_of: Vec<usize> = vec![usize::MAX; node_count];
    let mut stack: Vec<usize> = Vec::new();
    // #[gamma::skip(literal.int_increment, reason = "Tarjan discovery indices are compared only for order and equality, so translating every index by one is observationally identical")]
    let mut next_index: u32 = 0;
    // #[gamma::skip(literal.int_increment, reason = "component identifiers are internal labels, so starting at one merely leaves an unused zero component without changing reachability")]
    let mut next_component: usize = 0;

    for start in 0..node_count {
        if index_of[start].is_some() {
            continue;
        }

        let mut call_stack: Vec<Frame> = vec![Frame { node: start, edge_at: 0 }];
        index_of[start] = Some(next_index);
        low_link[start] = next_index;
        // #[gamma::skip(literal.int_increment, reason = "Tarjan discovery indices are compared only for order and equality, so using consecutive even indices is observationally identical")]
        next_index += 1;
        stack.push(start);
        on_stack[start] = true;

        while let Some(frame) = call_stack.last_mut() {
            let node = frame.node;

            if frame.edge_at < edges[node].len() {
                let neighbor = edges[node][frame.edge_at];
                // #[gamma::skip(stmt.delete_assign, reason = "without advancing the edge cursor Tarjan's traversal repeatedly processes the same edge and never terminates")]
                frame.edge_at += 1;

                if let Some(neighbor_index) = index_of[neighbor] {
                    if on_stack[neighbor] {
                        low_link[node] = low_link[node].min(neighbor_index);
                    }
                } else {
                    index_of[neighbor] = Some(next_index);
                    low_link[neighbor] = next_index;
                    next_index = next_discovery_index(next_index);
                    stack.push(neighbor);
                    on_stack[neighbor] = true;
                    call_stack.push(Frame {
                        node: neighbor,
                        edge_at: 0,
                    });
                }
            } else {
                call_stack.pop();

                // Guarded by pushing `start` above and never popping below it in this loop, so a
                // parent frame is always here to read the child's finished low-link from.
                if let Some(parent) = call_stack.last() {
                    low_link[parent.node] = low_link[parent.node].min(low_link[node]);
                }

                if low_link[node] == index_of[node].expect("just indexed above, on the way in") {
                    loop {
                        let member = stack.pop().expect("root of this component pushed before this loop started");
                        on_stack[member] = false;
                        comp_of[member] = next_component;

                        if member == node {
                            break;
                        }
                    }

                    // #[gamma::skip(literal.int_increment, reason = "gaps between internal component labels add only unused closure slots and do not change the represented reachability")]
                    next_component += 1;
                }
            }
        }
    }

    comp_of
}
