#[allow(dead_code)]
const MAX_KNOWN_MERCHANTS: usize = 64;

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct ParsedTransaction<'a> {
    pub amount: f32,
    pub installments: i32,
    pub requested_at: &'a [u8],
    pub customer_avg_amount: f32,
    pub tx_count_24h: i32,
    pub known_merchants: [&'a [u8]; MAX_KNOWN_MERCHANTS],
    pub known_merchants_len: usize,
    pub merchant_id: &'a [u8],
    pub merchant_mcc: usize,
    pub merchant_avg_amount: f32,
    pub is_online: bool,
    pub card_present: bool,
    pub km_from_home: f32,
    pub last_timestamp: Option<&'a [u8]>,
    pub last_km_from_current: f32,
}

impl<'a> Default for ParsedTransaction<'a> {
    fn default() -> Self {
        Self {
            amount: 0.0,
            installments: 0,
            requested_at: &[],
            customer_avg_amount: 0.0,
            tx_count_24h: 0,
            known_merchants: [&[]; MAX_KNOWN_MERCHANTS],
            known_merchants_len: 0,
            merchant_id: &[],
            merchant_mcc: usize::MAX,
            merchant_avg_amount: 0.0,
            is_online: false,
            card_present: false,
            km_from_home: 0.0,
            last_timestamp: None,
            last_km_from_current: 0.0,
        }
    }
}

#[allow(dead_code)]
pub fn parse_transaction(input: &[u8]) -> Option<ParsedTransaction<'_>> {
    let mut parser = Parser { input, pos: 0 };
    let mut out = ParsedTransaction::default();
    parser.parse_root(&mut out)?;

    if out.requested_at.is_empty() || out.merchant_id.is_empty() {
        return None;
    }
    Some(out)
}

#[allow(dead_code)]
struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn parse_root(&mut self, out: &mut ParsedTransaction<'a>) -> Option<()> {
        self.expect(b'{')?;
        loop {
            self.skip_ws();
            if self.consume(b'}') {
                return Some(());
            }
            let key = self.string()?;
            self.expect(b':')?;
            match key {
                b"transaction" => self.parse_transaction_object(out)?,
                b"customer" => self.parse_customer_object(out)?,
                b"merchant" => self.parse_merchant_object(out)?,
                b"terminal" => self.parse_terminal_object(out)?,
                b"last_transaction" => self.parse_last_transaction(out)?,
                _ => self.skip_value()?,
            }
            self.skip_ws();
            if self.consume(b',') {
                continue;
            }
            self.expect(b'}')?;
            return Some(());
        }
    }

    fn parse_transaction_object(&mut self, out: &mut ParsedTransaction<'a>) -> Option<()> {
        self.expect(b'{')?;
        loop {
            self.skip_ws();
            if self.consume(b'}') {
                return Some(());
            }
            let key = self.string()?;
            self.expect(b':')?;
            match key {
                b"amount" => out.amount = self.number()?,
                b"installments" => out.installments = self.integer()? as i32,
                b"requested_at" => out.requested_at = self.string()?,
                _ => self.skip_value()?,
            }
            self.object_sep_or_end()?;
            if self.input[self.pos - 1] == b'}' {
                return Some(());
            }
        }
    }

    fn parse_customer_object(&mut self, out: &mut ParsedTransaction<'a>) -> Option<()> {
        self.expect(b'{')?;
        loop {
            self.skip_ws();
            if self.consume(b'}') {
                return Some(());
            }
            let key = self.string()?;
            self.expect(b':')?;
            match key {
                b"avg_amount" => out.customer_avg_amount = self.number()?,
                b"tx_count_24h" => out.tx_count_24h = self.integer()? as i32,
                b"known_merchants" => self.parse_known_merchants(out)?,
                _ => self.skip_value()?,
            }
            self.object_sep_or_end()?;
            if self.input[self.pos - 1] == b'}' {
                return Some(());
            }
        }
    }

    fn parse_merchant_object(&mut self, out: &mut ParsedTransaction<'a>) -> Option<()> {
        self.expect(b'{')?;
        loop {
            self.skip_ws();
            if self.consume(b'}') {
                return Some(());
            }
            let key = self.string()?;
            self.expect(b':')?;
            match key {
                b"id" => out.merchant_id = self.string()?,
                b"mcc" => out.merchant_mcc = parse_usize_ascii(self.string()?),
                b"avg_amount" => out.merchant_avg_amount = self.number()?,
                _ => self.skip_value()?,
            }
            self.object_sep_or_end()?;
            if self.input[self.pos - 1] == b'}' {
                return Some(());
            }
        }
    }

    fn parse_terminal_object(&mut self, out: &mut ParsedTransaction<'a>) -> Option<()> {
        self.expect(b'{')?;
        loop {
            self.skip_ws();
            if self.consume(b'}') {
                return Some(());
            }
            let key = self.string()?;
            self.expect(b':')?;
            match key {
                b"is_online" => out.is_online = self.boolean()?,
                b"card_present" => out.card_present = self.boolean()?,
                b"km_from_home" => out.km_from_home = self.number()?,
                _ => self.skip_value()?,
            }
            self.object_sep_or_end()?;
            if self.input[self.pos - 1] == b'}' {
                return Some(());
            }
        }
    }

    fn parse_last_transaction(&mut self, out: &mut ParsedTransaction<'a>) -> Option<()> {
        self.skip_ws();
        if self.consume_literal(b"null") {
            out.last_timestamp = None;
            out.last_km_from_current = 0.0;
            return Some(());
        }

        self.expect(b'{')?;
        loop {
            self.skip_ws();
            if self.consume(b'}') {
                return Some(());
            }
            let key = self.string()?;
            self.expect(b':')?;
            match key {
                b"timestamp" => out.last_timestamp = Some(self.string()?),
                b"km_from_current" => out.last_km_from_current = self.number()?,
                _ => self.skip_value()?,
            }
            self.object_sep_or_end()?;
            if self.input[self.pos - 1] == b'}' {
                return Some(());
            }
        }
    }

    fn parse_known_merchants(&mut self, out: &mut ParsedTransaction<'a>) -> Option<()> {
        self.expect(b'[')?;
        loop {
            self.skip_ws();
            if self.consume(b']') {
                return Some(());
            }
            let merchant = self.string()?;
            if out.known_merchants_len < MAX_KNOWN_MERCHANTS {
                out.known_merchants[out.known_merchants_len] = merchant;
                out.known_merchants_len += 1;
            }
            self.skip_ws();
            if self.consume(b',') {
                continue;
            }
            self.expect(b']')?;
            return Some(());
        }
    }

    fn object_sep_or_end(&mut self) -> Option<()> {
        self.skip_ws();
        if self.consume(b',') || self.consume(b'}') {
            Some(())
        } else {
            None
        }
    }

    fn skip_value(&mut self) -> Option<()> {
        self.skip_ws();
        match self.peek()? {
            b'"' => {
                self.string()?;
                Some(())
            }
            b'{' => self.skip_object(),
            b'[' => self.skip_array(),
            b't' => self.consume_literal(b"true").then_some(()),
            b'f' => self.consume_literal(b"false").then_some(()),
            b'n' => self.consume_literal(b"null").then_some(()),
            _ => {
                self.number()?;
                Some(())
            }
        }
    }

    fn skip_object(&mut self) -> Option<()> {
        self.expect(b'{')?;
        loop {
            self.skip_ws();
            if self.consume(b'}') {
                return Some(());
            }
            self.string()?;
            self.expect(b':')?;
            self.skip_value()?;
            self.skip_ws();
            if self.consume(b',') {
                continue;
            }
            self.expect(b'}')?;
            return Some(());
        }
    }

    fn skip_array(&mut self) -> Option<()> {
        self.expect(b'[')?;
        loop {
            self.skip_ws();
            if self.consume(b']') {
                return Some(());
            }
            self.skip_value()?;
            self.skip_ws();
            if self.consume(b',') {
                continue;
            }
            self.expect(b']')?;
            return Some(());
        }
    }

    fn string(&mut self) -> Option<&'a [u8]> {
        self.skip_ws();
        self.expect(b'"')?;
        let start = self.pos;
        while self.pos < self.input.len() {
            match self.input[self.pos] {
                b'"' => {
                    let end = self.pos;
                    self.pos += 1;
                    return Some(&self.input[start..end]);
                }
                b'\\' => return None,
                _ => self.pos += 1,
            }
        }
        None
    }

    fn number(&mut self) -> Option<f32> {
        self.skip_ws();
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if self.pos == start {
            return None;
        }
        std::str::from_utf8(&self.input[start..self.pos])
            .ok()?
            .parse()
            .ok()
    }

    fn integer(&mut self) -> Option<i64> {
        self.number().map(|value| value as i64)
    }

    fn boolean(&mut self) -> Option<bool> {
        self.skip_ws();
        if self.consume_literal(b"true") {
            Some(true)
        } else if self.consume_literal(b"false") {
            Some(false)
        } else {
            None
        }
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        self.skip_ws();
        if self.consume(byte) {
            Some(())
        } else {
            None
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn consume_literal(&mut self, literal: &[u8]) -> bool {
        if self.input.get(self.pos..self.pos + literal.len()) == Some(literal) {
            self.pos += literal.len();
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.pos += 1;
        }
    }
}

fn parse_usize_ascii(bytes: &[u8]) -> usize {
    let mut out = 0usize;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return usize::MAX;
        }
        out = out * 10 + (byte - b'0') as usize;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = br#"{
      "terminal":{"km_from_home":29.23,"card_present":true,"is_online":false},
      "merchant":{"avg_amount":60.25,"mcc":"5411","id":"MERC-016"},
      "customer":{"known_merchants":["MERC-003","MERC-016"],"tx_count_24h":3,"avg_amount":82.24},
      "transaction":{"requested_at":"2026-03-11T18:45:53Z","installments":2,"amount":41.12},
      "id":"tx-1329056812",
      "last_transaction":null
    }"#;

    #[test]
    fn parses_payload_with_variable_order() {
        let parsed = parse_transaction(SAMPLE).unwrap();

        assert_eq!(parsed.amount, 41.12);
        assert_eq!(parsed.installments, 2);
        assert_eq!(parsed.requested_at, b"2026-03-11T18:45:53Z");
        assert_eq!(parsed.customer_avg_amount, 82.24);
        assert_eq!(parsed.tx_count_24h, 3);
        assert_eq!(parsed.merchant_id, b"MERC-016");
        assert_eq!(parsed.merchant_mcc, 5411);
        assert_eq!(parsed.merchant_avg_amount, 60.25);
        assert!(!parsed.is_online);
        assert!(parsed.card_present);
        assert_eq!(parsed.known_merchants_len, 2);
        assert_eq!(parsed.known_merchants[1], b"MERC-016");
        assert!(parsed.last_timestamp.is_none());
    }

    #[test]
    fn parses_last_transaction_object() {
        let payload = br#"{
          "transaction":{"amount":1,"installments":1,"requested_at":"2026-03-12T00:10:00Z"},
          "customer":{"avg_amount":2,"tx_count_24h":1,"known_merchants":[]},
          "merchant":{"id":"MERC-999","mcc":"5999","avg_amount":3},
          "terminal":{"is_online":true,"card_present":false,"km_from_home":4},
          "last_transaction":{"timestamp":"2026-03-11T23:50:00Z","km_from_current":5}
        }"#;

        let parsed = parse_transaction(payload).unwrap();
        assert_eq!(parsed.last_timestamp, Some(&b"2026-03-11T23:50:00Z"[..]));
        assert_eq!(parsed.last_km_from_current, 5.0);
    }
}
