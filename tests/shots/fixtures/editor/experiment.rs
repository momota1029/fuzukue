// Estimate the sample mean and retain an observation count.
#[derive(Debug)]
pub struct Summary {
    pub mean: f64,
    pub count: usize,
}

pub fn summarize(sample: &[f64]) -> Option<Summary> {
    if sample.is_empty() {
        return None;
    }
    let count = sample.len();
    let mean = sample.iter().sum::<f64>() / count as f64;
    Some(Summary { mean, count })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_mean() {
        let result = summarize(&[1.0, 3.0, 5.0]).unwrap();
        assert_eq!(result.mean, 3.0);
    }
}
