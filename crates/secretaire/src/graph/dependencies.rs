// Kosaraju's algorithm over requires edges. Iterative DFS handles long
// dependency chains without using the process stack.
pub(crate) fn requires_cycles<R, B>(
    nodes: impl IntoIterator<Item = usize>,
    node_count: usize,
    requires: impl Fn(usize) -> R,
    blocks: impl Fn(usize) -> B,
) -> Vec<Vec<usize>>
where
    R: Iterator<Item = usize>,
    B: Iterator<Item = usize>,
{
    let mut visited = vec![false; node_count];
    let mut finished = Vec::new();
    for start in nodes {
        if std::mem::replace(&mut visited[start], true) {
            continue;
        }
        let mut stack = vec![(start, requires(start))];
        while let Some((node, edges)) = stack.last_mut() {
            if let Some(target) = edges.next() {
                if !std::mem::replace(&mut visited[target], true) {
                    stack.push((target, requires(target)));
                }
            } else {
                finished.push(*node);
                stack.pop();
            }
        }
    }
    visited.fill(false);
    let mut cycles = Vec::new();
    for start in finished.into_iter().rev() {
        if std::mem::replace(&mut visited[start], true) {
            continue;
        }
        let mut component = Vec::new();
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            component.push(node);
            for source in blocks(node) {
                if !std::mem::replace(&mut visited[source], true) {
                    stack.push(source);
                }
            }
        }
        if component.len() > 1 || requires(start).any(|target| target == start) {
            component.sort_unstable();
            cycles.push(component);
        }
    }
    cycles.sort_unstable();
    cycles
}
