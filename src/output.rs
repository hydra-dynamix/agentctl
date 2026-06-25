pub fn bounded_output(output: &str, max_output_bytes: usize, tail_bytes: usize) -> String {
    if output.len() <= max_output_bytes {
        return output.to_owned();
    }

    let keep_head = max_output_bytes.saturating_sub(tail_bytes);
    let head = clamp_to_char_boundary(output, keep_head);
    let tail_start = output.len().saturating_sub(tail_bytes);
    let tail_start = clamp_start_to_char_boundary(output, tail_start);
    format!(
        "{}\n\n[... omitted {} bytes ...]\n\n{}",
        &output[..head],
        output
            .len()
            .saturating_sub(head + (output.len() - tail_start)),
        &output[tail_start..]
    )
}

fn clamp_to_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn clamp_start_to_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

pub fn combine_output(stdout: &str, stderr: &str) -> String {
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("stdout:\n{}", stdout),
        (true, false) => format!("stderr:\n{}", stderr),
        (false, false) => format!("stdout:\n{}\nstderr:\n{}", stdout.trim_end(), stderr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_output_clamps_head_on_utf8_boundary() {
        let output = format!("ab{}cdef", "é");

        let bounded = bounded_output(&output, 5, 2);

        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.starts_with("ab"));
        assert!(bounded.ends_with("ef"));
    }

    #[test]
    fn bounded_output_clamps_tail_on_utf8_boundary() {
        let output = format!("abcd{}ef", "é");

        let bounded = bounded_output(&output, 5, 4);

        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.starts_with("a"));
        assert!(bounded.ends_with("éef"));
    }
}
