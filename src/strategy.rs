use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Buy,
    Sell,
}

pub struct EmaCrossover {
    fast: usize,
    slow: usize,
    fast_ema: Option<f64>,
    slow_ema: Option<f64>,
    samples: usize,
}

impl EmaCrossover {
    pub fn new(fast: usize, slow: usize) -> Result<Self> {
        if fast == 0 || fast >= slow {
            bail!("EMA windows must satisfy 0 < fast < slow");
        }
        Ok(Self {
            fast,
            slow,
            fast_ema: None,
            slow_ema: None,
            samples: 0,
        })
    }

    pub fn seed(&mut self, closes: Vec<f64>) {
        for close in closes {
            self.update(close);
        }
    }

    pub fn on_close(&mut self, price: f64) -> Option<Signal> {
        let previous = self.averages();
        self.update(price);
        let current = self.averages();
        match (previous, current) {
            (Some((pf, ps)), Some((f, s))) if pf <= ps && f > s => Some(Signal::Buy),
            (Some((pf, ps)), Some((f, s))) if pf >= ps && f < s => Some(Signal::Sell),
            _ => None,
        }
    }

    pub fn averages(&self) -> Option<(f64, f64)> {
        if self.samples < self.slow {
            return None;
        }
        Some((self.fast_ema?, self.slow_ema?))
    }

    fn update(&mut self, price: f64) {
        self.fast_ema = Some(next_ema(self.fast_ema, price, self.fast));
        self.slow_ema = Some(next_ema(self.slow_ema, price, self.slow));
        self.samples += 1;
    }
}

fn next_ema(previous: Option<f64>, price: f64, period: usize) -> f64 {
    previous.map_or(price, |ema| {
        let multiplier = 2.0 / (period as f64 + 1.0);
        (price - ema) * multiplier + ema
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_buy_on_upward_cross() {
        let mut strategy = EmaCrossover::new(2, 3).unwrap();
        strategy.seed(vec![4.0, 3.0, 2.0]);
        assert_eq!(strategy.on_close(5.0), Some(Signal::Buy));
    }

    #[test]
    fn waits_for_enough_prices() {
        let mut strategy = EmaCrossover::new(2, 3).unwrap();
        assert_eq!(strategy.on_close(1.0), None);
        assert_eq!(strategy.on_close(2.0), None);
    }

    #[test]
    fn calculates_expected_ema() {
        let mut strategy = EmaCrossover::new(2, 3).unwrap();
        strategy.seed(vec![10.0, 11.0, 12.0]);
        let (fast, slow) = strategy.averages().unwrap();
        assert!((fast - 11.555_555).abs() < 0.000_01);
        assert!((slow - 11.25).abs() < 0.000_01);
    }
}
