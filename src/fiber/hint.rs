//! Invoices with a trampoline route hint.
//!
//! Wallets behind an LSP (Cellula, for one) put a `TrampolineRouteHint` attribute
//! (union id 10, from nervosnetwork/fiber#1623) in their invoices: the public node that
//! reaches the private payee. Fiber through 0.10.0-rc3 does not know that attribute, so
//! `parse_invoice` and `send_payment` with the invoice both refuse it. Those builds can
//! still pay it by fields with `trampoline_hops`, so this module reads such an invoice
//! locally and the payment calls pay it that way.
//!
//! Only used after Fiber refuses an invoice with an unknown attribute. The signature is
//! not checked here: the payee is the buyer's own wallet and a wrong one only misdirects
//! the buyer's payout.
//!
//! Drop this module once Fiber past 0.10.0-rc3 parses attr 10 (fiber#1623).

use anyhow::{Result, anyhow, bail};
use arcode::bitbit::{BitReader, MSB};
use arcode::{ArithmeticDecoder, EOFKind, Model};
use bech32::{FromBase32, Variant};

/// Fiber `SIGNATURE_U5_SIZE`.
const SIGNATURE_U5_SIZE: usize = 104;
/// Fiber `MAX_INVOICE_DATA_LENGTH` is larger; an invoice needs a few hundred bytes.
const MAX_DATA_LENGTH: usize = 4096;

const ATTR_EXPIRY_TIME: u32 = 0;
const ATTR_FINAL_HTLC_MINIMUM_EXPIRY_DELTA: u32 = 3;
const ATTR_PAYEE_PUBLIC_KEY: u32 = 7;
const ATTR_HASH_ALGORITHM: u32 = 8;
const ATTR_TRAMPOLINE_ROUTE_HINT: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintedInvoice {
    /// `Fibb`, `Fibt` or `Fibd`, as Fiber's `parse_invoice` names it.
    pub currency: String,
    pub amount: Option<u128>,
    pub timestamp_ms: u128,
    pub payment_hash: String,
    pub expiry_secs: Option<u64>,
    pub final_tlc_expiry_delta_ms: Option<u64>,
    pub payee: String,
    /// 0 ckb hash, 1 sha256.
    pub hash_algorithm: u8,
    pub trampoline_hint: String,
}

/// Reads an invoice that carries a trampoline route hint. Errors for anything else,
/// so a plain invoice keeps going through Fiber.
pub fn decode_hinted_invoice(invoice: &str) -> Result<HintedInvoice> {
    let invoice = invoice.trim();
    let (hrp, data, variant) = bech32::decode(invoice).map_err(|e| anyhow!("bech32: {e}"))?;
    if variant != Variant::Bech32m {
        bail!("not a bech32m invoice");
    }
    let (currency, amount) = parse_hrp(&hrp)?;
    if data.len() <= SIGNATURE_U5_SIZE + 1 {
        bail!("invoice data too short");
    }
    let signed = data[0].to_u8() == 1;
    if !signed {
        bail!("invoice is not signed");
    }
    let body = Vec::<u8>::from_base32(&data[1..data.len() - SIGNATURE_U5_SIZE])
        .map_err(|e| anyhow!("base32: {e}"))?;
    let raw = decompress(&body)?;
    let (timestamp_ms, payment_hash, attrs) = read_raw_invoice_data(&raw)?;

    let mut out = HintedInvoice {
        currency,
        amount,
        timestamp_ms,
        payment_hash: format!("0x{}", hex::encode(payment_hash)),
        expiry_secs: None,
        final_tlc_expiry_delta_ms: None,
        payee: String::new(),
        hash_algorithm: 0,
        trampoline_hint: String::new(),
    };
    for (id, item) in attrs {
        match id {
            ATTR_EXPIRY_TIME => out.expiry_secs = Some(u64_le(item)?),
            ATTR_FINAL_HTLC_MINIMUM_EXPIRY_DELTA => {
                out.final_tlc_expiry_delta_ms = Some(u64_le(item)?)
            }
            ATTR_PAYEE_PUBLIC_KEY => out.payee = pubkey(table_bytes(item)?)?,
            ATTR_HASH_ALGORITHM => {
                out.hash_algorithm = *item
                    .first()
                    .ok_or_else(|| anyhow!("empty hash algorithm"))?
            }
            ATTR_TRAMPOLINE_ROUTE_HINT => out.trampoline_hint = pubkey(table_bytes(item)?)?,
            _ => {}
        }
    }
    if out.trampoline_hint.is_empty() {
        bail!("invoice has no trampoline route hint");
    }
    if out.payee.is_empty() {
        bail!("invoice has no payee public key");
    }
    Ok(out)
}

fn parse_hrp(hrp: &str) -> Result<(String, Option<u128>)> {
    let rest = hrp
        .strip_prefix("fib")
        .ok_or_else(|| anyhow!("not a Fiber invoice"))?;
    let mut chars = rest.chars();
    let currency = match chars.next() {
        Some('b') => "Fibb",
        Some('t') => "Fibt",
        Some('d') => "Fibd",
        _ => bail!("unknown invoice currency"),
    };
    let digits = chars.as_str();
    let amount = if digits.is_empty() {
        None
    } else {
        Some(
            digits
                .parse::<u128>()
                .map_err(|_| anyhow!("bad invoice amount"))?,
        )
    };
    Ok((currency.to_string(), amount))
}

/// Fiber's `ar_decompress`: 8-bit adaptive model, EOF symbol added at the end.
fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut model = Model::builder().num_bits(8).eof(EOFKind::EndAddOne).build();
    let mut reader = BitReader::<_, MSB>::new(data);
    let mut decoder = ArithmeticDecoder::new(48);
    let mut out = Vec::new();
    while !decoder.finished() {
        let sym = decoder
            .decode(&model, &mut reader)
            .map_err(|e| anyhow!("decompress: {e}"))?;
        model.update_symbol(sym);
        out.push(sym as u8);
        if out.len() > MAX_DATA_LENGTH {
            bail!("invoice data too long");
        }
    }
    out.pop().ok_or_else(|| anyhow!("missing EOF marker"))?;
    Ok(out)
}

type Attrs<'a> = Vec<(u32, &'a [u8])>;

/// `table RawInvoiceData { timestamp: Uint128, payment_hash: PaymentHash, attrs: InvoiceAttrsVec }`
fn read_raw_invoice_data(raw: &[u8]) -> Result<(u128, &[u8], Attrs<'_>)> {
    let fields = table_fields(raw, 3)?;
    let timestamp = fields[0];
    if timestamp.len() != 16 {
        bail!("bad timestamp");
    }
    let timestamp_ms = u128::from_le_bytes(timestamp.try_into().expect("16 bytes"));
    let payment_hash = fields[1];
    if payment_hash.len() != 32 {
        bail!("bad payment hash");
    }
    let mut attrs = Vec::new();
    for item in dynvec_items(fields[2])? {
        if item.len() < 4 {
            bail!("bad invoice attribute");
        }
        let id = u32::from_le_bytes(item[..4].try_into().expect("4 bytes"));
        attrs.push((id, &item[4..]));
    }
    Ok((timestamp_ms, payment_hash, attrs))
}

/// Molecule table or dynvec: total size, then one offset per field.
fn offsets(buf: &[u8]) -> Result<Vec<usize>> {
    let total = read_u32(buf, 0)? as usize;
    if total != buf.len() {
        bail!("molecule size mismatch");
    }
    if total == 4 {
        return Ok(Vec::new());
    }
    let first = read_u32(buf, 4)? as usize;
    if !first.is_multiple_of(4) || first < 8 || first > total {
        bail!("bad molecule header");
    }
    let mut out = Vec::with_capacity(first / 4 - 1);
    for i in 1..first / 4 {
        out.push(read_u32(buf, i * 4)? as usize);
    }
    out.push(total);
    if out.windows(2).any(|w| w[0] > w[1]) {
        bail!("bad molecule offsets");
    }
    Ok(out)
}

fn table_fields(buf: &[u8], at_least: usize) -> Result<Vec<&[u8]>> {
    let offs = offsets(buf)?;
    if offs.len() < at_least + 1 {
        bail!("molecule table has too few fields");
    }
    Ok(offs.windows(2).map(|w| &buf[w[0]..w[1]]).collect())
}

fn dynvec_items(buf: &[u8]) -> Result<Vec<&[u8]>> {
    let offs = offsets(buf)?;
    Ok(offs.windows(2).map(|w| &buf[w[0]..w[1]]).collect())
}

/// `table { value: Bytes }`: the bytes of its one field.
fn table_bytes(buf: &[u8]) -> Result<&[u8]> {
    let fields = table_fields(buf, 1)?;
    let bytes = fields[0];
    let len = read_u32(bytes, 0)? as usize;
    if bytes.len() != 4 + len {
        bail!("bad molecule bytes");
    }
    Ok(&bytes[4..])
}

fn pubkey(bytes: &[u8]) -> Result<String> {
    if bytes.len() != 33 || !(bytes[0] == 2 || bytes[0] == 3) {
        bail!("bad public key");
    }
    Ok(hex::encode(bytes))
}

fn u64_le(item: &[u8]) -> Result<u64> {
    Ok(u64::from_le_bytes(
        item.try_into().map_err(|_| anyhow!("bad u64 attribute"))?,
    ))
}

fn read_u32(buf: &[u8], at: usize) -> Result<u32> {
    let bytes = buf
        .get(at..at + 4)
        .ok_or_else(|| anyhow!("molecule data too short"))?;
    Ok(u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
}

#[cfg(test)]
pub(crate) mod test_invoice {
    use super::*;
    use arcode::bitbit::BitWriter;
    use arcode::ArithmeticEncoder;
    use bech32::{ToBase32, encode, u5};
    use std::io::Cursor;

    /// Test invoice with attr 10. Signature is zeros (decoder does not verify it).
    pub(crate) fn encode_hinted_invoice(
        hrp: &str,
        timestamp_ms: u128,
        payment_hash: &[u8; 32],
        expiry_secs: Option<u64>,
        final_tlc_expiry_delta_ms: Option<u64>,
        payee: &[u8; 33],
        hash_algorithm: u8,
        trampoline: &[u8; 33],
    ) -> String {
        let mut attrs = Vec::new();
        if let Some(secs) = expiry_secs {
            attrs.push(encode_union(ATTR_EXPIRY_TIME, &secs.to_le_bytes()));
        }
        if let Some(delta) = final_tlc_expiry_delta_ms {
            attrs.push(encode_union(
                ATTR_FINAL_HTLC_MINIMUM_EXPIRY_DELTA,
                &delta.to_le_bytes(),
            ));
        }
        attrs.push(encode_union(ATTR_PAYEE_PUBLIC_KEY, &encode_pubkey_attr(payee)));
        attrs.push(encode_union(ATTR_HASH_ALGORITHM, &[hash_algorithm]));
        attrs.push(encode_union(
            ATTR_TRAMPOLINE_ROUTE_HINT,
            &encode_pubkey_attr(trampoline),
        ));

        let raw = encode_dynvec(&[
            timestamp_ms.to_le_bytes().to_vec(),
            payment_hash.to_vec(),
            encode_dynvec(&attrs),
        ]);
        let compressed = compress(&raw).expect("compress");

        let mut data = vec![u5::try_from_u8(1).expect("signed flag")];
        data.extend(compressed.to_base32());
        data.extend(std::iter::repeat_n(
            u5::try_from_u8(0).expect("sig"),
            SIGNATURE_U5_SIZE,
        ));
        encode(hrp, data, Variant::Bech32m).expect("bech32m")
    }

    fn compress(data: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut model = Model::builder().num_bits(8).eof(EOFKind::EndAddOne).build();
        let mut compressed_writer = BitWriter::new(Cursor::new(vec![]));
        let mut encoder = ArithmeticEncoder::new(48);
        for &sym in data {
            encoder.encode(u32::from(sym), &model, &mut compressed_writer)?;
            model.update_symbol(u32::from(sym));
        }
        encoder.encode(model.eof(), &model, &mut compressed_writer)?;
        encoder.finish_encode(&mut compressed_writer)?;
        compressed_writer.pad_to_byte()?;
        Ok(compressed_writer.get_ref().get_ref().clone())
    }

    fn encode_pubkey_attr(bytes: &[u8]) -> Vec<u8> {
        let mut field = Vec::with_capacity(4 + bytes.len());
        field.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        field.extend_from_slice(bytes);
        encode_dynvec(&[field])
    }

    fn encode_union(id: u32, content: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + content.len());
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(content);
        out
    }

    fn encode_dynvec(items: &[Vec<u8>]) -> Vec<u8> {
        let header = 4 * (items.len() + 1);
        let mut offsets = Vec::with_capacity(items.len());
        let mut cursor = header;
        for item in items {
            offsets.push(cursor);
            cursor += item.len();
        }
        let total = cursor;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_le_bytes());
        for offset in offsets {
            out.extend_from_slice(&(offset as u32).to_le_bytes());
        }
        for item in items {
            out.extend_from_slice(item);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_invoice::encode_hinted_invoice;
    use super::*;

    #[test]
    fn hrp_reads_currency_and_amount() {
        assert_eq!(
            parse_hrp("fibt10000000000").unwrap(),
            ("Fibt".to_string(), Some(10_000_000_000))
        );
        assert_eq!(parse_hrp("fibb").unwrap(), ("Fibb".to_string(), None));
        assert!(parse_hrp("lnbc1").is_err());
    }

    #[test]
    fn plain_text_is_not_an_invoice() {
        assert!(decode_hinted_invoice("hello").is_err());
    }

    #[test]
    fn trampoline_invoice_round_trips() {
        let payment_hash = [0x11u8; 32];
        let mut payee = [0x22u8; 33];
        payee[0] = 0x02;
        let mut trampoline = [0x33u8; 33];
        trampoline[0] = 0x03;
        let invoice = encode_hinted_invoice(
            "fibt10000000000",
            1_700_000_000_000,
            &payment_hash,
            Some(172_800),
            Some(86_400_000),
            &payee,
            0,
            &trampoline,
        );
        let decoded = decode_hinted_invoice(&invoice).unwrap();
        assert_eq!(decoded.currency, "Fibt");
        assert_eq!(decoded.amount, Some(10_000_000_000));
        assert_eq!(decoded.timestamp_ms, 1_700_000_000_000);
        assert_eq!(
            decoded.payment_hash,
            format!("0x{}", hex::encode(payment_hash))
        );
        assert_eq!(decoded.expiry_secs, Some(172_800));
        assert_eq!(decoded.final_tlc_expiry_delta_ms, Some(86_400_000));
        assert_eq!(decoded.payee, hex::encode(payee));
        assert_eq!(decoded.hash_algorithm, 0);
        assert_eq!(decoded.trampoline_hint, hex::encode(trampoline));
    }
}
