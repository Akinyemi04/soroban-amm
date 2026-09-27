package gosdk

import (
	"bytes"
	"encoding/hex"
	"errors"
	"math/big"
	"strings"
	"testing"
)

// ── Primitive writer/reader ───────────────────────────────────────────────────

func TestXDRWriterPrimitivesAreBigEndian(t *testing.T) {
	cases := []struct {
		name  string
		write func(w *xdrWriter)
		want  string
	}{
		{"uint32 zero", func(w *xdrWriter) { w.writeUint32(0) }, "00000000"},
		{"uint32 max", func(w *xdrWriter) { w.writeUint32(^uint32(0)) }, "ffffffff"},
		{"uint32 order", func(w *xdrWriter) { w.writeUint32(0x01020304) }, "01020304"},
		{"int32 minus one", func(w *xdrWriter) { w.writeInt32(-1) }, "ffffffff"},
		{"int32 min", func(w *xdrWriter) { w.writeInt32(-2_147_483_648) }, "80000000"},
		{"uint64 order", func(w *xdrWriter) { w.writeUint64(0x0102030405060708) }, "0102030405060708"},
		{"int64 min", func(w *xdrWriter) { w.writeInt64(-9_223_372_036_854_775_808) }, "8000000000000000"},
		{"bool true", func(w *xdrWriter) { w.writeBool(true) }, "00000001"},
		{"bool false", func(w *xdrWriter) { w.writeBool(false) }, "00000000"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			var w xdrWriter
			tc.write(&w)
			if got := hex.EncodeToString(w.bytes()); got != tc.want {
				t.Fatalf("encoded = %s, want %s", got, tc.want)
			}
		})
	}
}

func TestXDRWriterPadsOpaqueToFourBytes(t *testing.T) {
	for n := 0; n <= 9; n++ {
		t.Run("len "+itoa(n), func(t *testing.T) {
			payload := bytes.Repeat([]byte{0xab}, n)

			var raw xdrWriter
			raw.writeRaw(payload)
			wantLen := (n + 3) / 4 * 4
			if len(raw.bytes()) != wantLen {
				t.Fatalf("writeRaw wrote %d bytes, want %d", len(raw.bytes()), wantLen)
			}
			for i, b := range raw.bytes()[n:] {
				if b != 0 {
					t.Fatalf("padding byte %d = %#x, want 0", i, b)
				}
			}

			var prefixed xdrWriter
			prefixed.writeBytes(payload)
			if len(prefixed.bytes()) != 4+wantLen {
				t.Fatalf("writeBytes wrote %d bytes, want %d", len(prefixed.bytes()), 4+wantLen)
			}

			r := &xdrReader{buf: prefixed.bytes()}
			got, err := r.readBytes()
			if err != nil {
				t.Fatalf("readBytes: %v", err)
			}
			if !bytes.Equal(got, payload) {
				t.Fatalf("readBytes = %x, want %x", got, payload)
			}
			if r.pos != len(prefixed.bytes()) {
				t.Fatalf("reader stopped at %d, want %d (padding must be consumed)", r.pos, len(prefixed.bytes()))
			}
		})
	}
}

func TestXDRReaderPrimitivesRoundTrip(t *testing.T) {
	var w xdrWriter
	w.writeUint32(0xdeadbeef)
	w.writeInt32(-7)
	w.writeUint64(^uint64(0))
	w.writeInt64(-9_223_372_036_854_775_808)

	r := &xdrReader{buf: w.bytes()}
	if v, err := r.readUint32(); err != nil || v != 0xdeadbeef {
		t.Fatalf("readUint32 = %#x, %v", v, err)
	}
	if v, err := r.readInt32(); err != nil || v != -7 {
		t.Fatalf("readInt32 = %d, %v", v, err)
	}
	if v, err := r.readUint64(); err != nil || v != ^uint64(0) {
		t.Fatalf("readUint64 = %d, %v", v, err)
	}
	if v, err := r.readUint64(); err != nil || int64(v) != -9_223_372_036_854_775_808 {
		t.Fatalf("readUint64 (as int64) = %d, %v", int64(v), err)
	}
	if r.pos != len(r.buf) {
		t.Fatalf("reader stopped at %d, want %d", r.pos, len(r.buf))
	}
}

func TestXDRReaderRejectsTruncation(t *testing.T) {
	cases := []struct {
		name string
		buf  string
		read func(r *xdrReader) error
	}{
		{"uint32 from empty", "", func(r *xdrReader) error { _, err := r.readUint32(); return err }},
		{"uint32 from 3 bytes", "000000", func(r *xdrReader) error { _, err := r.readUint32(); return err }},
		{"int32 from 2 bytes", "0000", func(r *xdrReader) error { _, err := r.readInt32(); return err }},
		{"uint64 missing low limb", "00000001", func(r *xdrReader) error { _, err := r.readUint64(); return err }},
		{"uint64 short low limb", "00000001000000", func(r *xdrReader) error { _, err := r.readUint64(); return err }},
		{"raw missing padding", "616263", func(r *xdrReader) error { _, err := r.readRaw(3); return err }},
		{"raw fixed 32 from 31", strings.Repeat("00", 31), func(r *xdrReader) error { _, err := r.readRaw(32); return err }},
		{"bytes missing length", "", func(r *xdrReader) error { _, err := r.readBytes(); return err }},
		{"bytes length beyond buffer", "00000008" + "00000000", func(r *xdrReader) error { _, err := r.readBytes(); return err }},
		{"bytes missing padding", "00000003" + "616263", func(r *xdrReader) error { _, err := r.readBytes(); return err }},
		{"length max uint32", "ffffffff", func(r *xdrReader) error { _, err := r.readLength(); return err }},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			buf, err := hex.DecodeString(tc.buf)
			if err != nil {
				t.Fatalf("bad test hex: %v", err)
			}
			if err := tc.read(&xdrReader{buf: buf}); !errors.Is(err, ErrXDR) {
				t.Fatalf("expected ErrXDR, got %v", err)
			}
		})
	}
}

func TestXDRReaderLengthAcceptsExactRemainder(t *testing.T) {
	// A length equal to the remaining bytes is valid; only one past it is not.
	r := &xdrReader{buf: []byte{0, 0, 0, 4, 1, 2, 3, 4}}
	got, err := r.readBytes()
	if err != nil {
		t.Fatalf("readBytes: %v", err)
	}
	if !bytes.Equal(got, []byte{1, 2, 3, 4}) {
		t.Fatalf("readBytes = %x", got)
	}
}

// ── ScVal wire format ─────────────────────────────────────────────────────────

// TestScValWireFormat pins the exact bytes for each supported type, so an
// encoder and decoder that agree with each other but not with the network are
// still caught.
func TestScValWireFormat(t *testing.T) {
	zero32 := strings.Repeat("00", 32)
	cases := []struct {
		name string
		val  ScValue
		want string
	}{
		{"bool true", Bool(true), "00000000" + "00000001"},
		{"bool false", Bool(false), "00000000" + "00000000"},
		{"void", Void(), "00000001"},
		{"u32 max", U32(^uint32(0)), "00000003" + "ffffffff"},
		{"i32 minus one", I32(-1), "00000004" + "ffffffff"},
		{"u64 max", U64(^uint64(0)), "00000005" + "ffffffffffffffff"},
		{"i64 min", I64(-9_223_372_036_854_775_808), "00000006" + "8000000000000000"},
		{"u128 max", U128(new(big.Int).Set(MaxU128)), "00000009" + strings.Repeat("ff", 16)},
		{"u128 two to the 64", U128(new(big.Int).Lsh(big.NewInt(1), 64)), "00000009" + "0000000000000001" + "0000000000000000"},
		{"i128 zero", I128(big.NewInt(0)), "0000000a" + strings.Repeat("00", 16)},
		{"i128 minus one", I128(big.NewInt(-1)), "0000000a" + strings.Repeat("ff", 16)},
		{"i128 max", I128(new(big.Int).Set(MaxI128)), "0000000a" + "7fffffffffffffff" + "ffffffffffffffff"},
		{"i128 min", I128(new(big.Int).Set(MinI128)), "0000000a" + "8000000000000000" + "0000000000000000"},
		{"bytes empty", Bytes(nil), "0000000e" + "00000000"},
		{"bytes padded", Bytes([]byte{1, 2, 3, 4, 5}), "0000000e" + "00000005" + "0102030405000000"},
		{"string empty", Str(""), "0000000f" + "00000000"},
		{"string", Str("abc"), "0000000f" + "00000003" + "61626300"},
		{"symbol", Symbol("swap"), "00000010" + "00000004" + "73776170"},
		{"vec empty", Vec(), "00000011" + "00000001" + "00000000"},
		{"vec", Vec(U32(7), Bool(true)), "00000011" + "00000001" + "00000002" + "00000003" + "00000007" + "00000000" + "00000001"},
		{"map empty", Map(), "00000012" + "00000001" + "00000000"},
		{"map", Map(ScMapEntry{Key: Symbol("a"), Val: U32(1)}), "00000012" + "00000001" + "00000001" + "00000010" + "00000001" + "61000000" + "00000003" + "00000001"},
		{"account address", Addr(zeroAccount), "00000013" + "00000000" + "00000000" + zero32},
		{"contract address", Addr(zeroContract), "00000013" + "00000001" + zero32},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			encoded, err := EncodeScVal(tc.val)
			if err != nil {
				t.Fatalf("encode: %v", err)
			}
			if got := hex.EncodeToString(encoded); got != tc.want {
				t.Fatalf("encoded = %s\n            want %s", got, tc.want)
			}

			want, _ := hex.DecodeString(tc.want)
			decoded, err := DecodeScVal(want)
			if err != nil {
				t.Fatalf("decode: %v", err)
			}
			assertScValEqual(t, decoded, tc.val)
		})
	}
}

// TestScValBoundaryRoundTrip covers the edges of each type's range, the
// padding boundaries of opaque data, and the symbol length limit.
func TestScValBoundaryRoundTrip(t *testing.T) {
	twoTo64 := new(big.Int).Lsh(big.NewInt(1), 64)
	maxSymbol := strings.Repeat("s", 32)
	longString := strings.Repeat("x", 1025)

	cases := []struct {
		name string
		val  ScValue
	}{
		{"u32 zero", U32(0)},
		{"u32 max", U32(^uint32(0))},
		{"i32 max", I32(2_147_483_647)},
		{"i32 min", I32(-2_147_483_648)},
		{"u64 zero", U64(0)},
		{"u64 max", U64(^uint64(0))},
		{"i64 max", I64(9_223_372_036_854_775_807)},
		{"i64 min", I64(-9_223_372_036_854_775_808)},
		{"i128 zero", I128(big.NewInt(0))},
		{"i128 max", I128(new(big.Int).Set(MaxI128))},
		{"i128 min", I128(new(big.Int).Set(MinI128))},
		{"i128 max minus one", I128(new(big.Int).Sub(MaxI128, big.NewInt(1)))},
		{"i128 min plus one", I128(new(big.Int).Add(MinI128, big.NewInt(1)))},
		{"i128 two to the 64", I128(new(big.Int).Set(twoTo64))},
		{"i128 minus two to the 64", I128(new(big.Int).Neg(twoTo64))},
		{"i128 u64 max", I128(new(big.Int).SetUint64(^uint64(0)))},
		{"u128 zero", U128(big.NewInt(0))},
		{"u128 max", U128(new(big.Int).Set(MaxU128))},
		{"u128 two to the 64", U128(new(big.Int).Set(twoTo64))},
		{"bytes 1", Bytes([]byte{1})},
		{"bytes 3", Bytes([]byte{1, 2, 3})},
		{"bytes 4", Bytes([]byte{1, 2, 3, 4})},
		{"bytes 5", Bytes([]byte{1, 2, 3, 4, 5})},
		{"bytes all zero", Bytes(make([]byte, 32))},
		{"string empty", Str("")},
		{"string long", Str(longString)},
		{"string utf8", Str("héllo, 世界")},
		{"symbol empty", Symbol("")},
		{"symbol one char", Symbol("a")},
		{"symbol max length", Symbol(maxSymbol)},
		{"vec empty", Vec()},
		{"vec of empty vecs", Vec(Vec(), Vec(), Vec())},
		{"vec mixed extremes", Vec(I128(new(big.Int).Set(MinI128)), U128(new(big.Int).Set(MaxU128)), Symbol(maxSymbol), Bytes(nil), Void())},
		{"deeply nested vec", Vec(Vec(Vec(Vec(Vec(U32(1))))))},
		{"map empty", Map()},
		{"map with vec values", Map(
			ScMapEntry{Key: Symbol("path"), Val: Vec(Addr(zeroContract), Addr(zeroContract))},
			ScMapEntry{Key: Symbol("amounts"), Val: Vec(I128(big.NewInt(1)), I128(big.NewInt(-1)))},
		)},
		{"map with non-symbol keys", Map(
			ScMapEntry{Key: U32(1), Val: Str("one")},
			ScMapEntry{Key: Addr(zeroAccount), Val: Bool(true)},
		)},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			encoded, err := EncodeScVal(tc.val)
			if err != nil {
				t.Fatalf("encode: %v", err)
			}
			if len(encoded)%4 != 0 {
				t.Fatalf("encoding is %d bytes, not a multiple of 4", len(encoded))
			}
			got, err := DecodeScVal(encoded)
			if err != nil {
				t.Fatalf("decode: %v", err)
			}
			assertScValEqual(t, got, tc.val)

			// Re-encoding the decoded value must be byte-identical.
			again, err := EncodeScVal(got)
			if err != nil {
				t.Fatalf("re-encode: %v", err)
			}
			if !bytes.Equal(again, encoded) {
				t.Fatalf("re-encoding differs:\n got %x\nwant %x", again, encoded)
			}
		})
	}
}

func TestEncodeScValRejects(t *testing.T) {
	cases := []struct {
		name     string
		val      ScValue
		sentinel error
	}{
		{"symbol 33 chars", Symbol(strings.Repeat("s", 33)), ErrScValue},
		{"i128 max plus one", I128(new(big.Int).Add(MaxI128, big.NewInt(1))), ErrI128OutOfRange},
		{"i128 min minus one", I128(new(big.Int).Sub(MinI128, big.NewInt(1))), ErrI128OutOfRange},
		{"i128 nil", I128(nil), ErrI128OutOfRange},
		{"u128 max plus one", U128(new(big.Int).Add(MaxU128, big.NewInt(1))), ErrU128OutOfRange},
		{"u128 negative", U128(big.NewInt(-1)), ErrU128OutOfRange},
		{"u128 nil", U128(nil), ErrU128OutOfRange},
		{"address empty", Addr(""), ErrXDR},
		{"address bad checksum", Addr(zeroAccount[:55] + "A"), ErrXDR},
		{"error type", ScValue{Type: scvError}, ErrScValue},
		{"unknown type", ScValue{Type: 99}, ErrScValue},
		{"bad element inside vec", Vec(U32(1), I128(nil)), ErrI128OutOfRange},
		{"bad key inside map", Map(ScMapEntry{Key: Symbol(strings.Repeat("k", 40)), Val: Void()}), ErrScValue},
		{"bad value inside map", Map(ScMapEntry{Key: Symbol("k"), Val: Addr("nope")}), ErrXDR},
		{"bad element nested deep", Vec(Vec(Vec(U128(big.NewInt(-5))))), ErrU128OutOfRange},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if _, err := EncodeScVal(tc.val); !errors.Is(err, tc.sentinel) {
				t.Fatalf("expected %v, got %v", tc.sentinel, err)
			}
			if _, err := EncodeScValBase64(tc.val); !errors.Is(err, tc.sentinel) {
				t.Fatalf("base64: expected %v, got %v", tc.sentinel, err)
			}
		})
	}
}

func TestDecodeScValRejects(t *testing.T) {
	zero32 := strings.Repeat("00", 32)
	cases := []struct {
		name     string
		hex      string
		sentinel error
	}{
		{"empty buffer", "", ErrXDR},
		{"type only, bool body missing", "00000000", ErrXDR},
		{"u64 body truncated", "00000005" + "00000000", ErrXDR},
		{"i128 low limb missing", "0000000a" + "0000000000000000", ErrXDR},
		{"bytes length beyond buffer", "0000000e" + "00000010" + "00000000", ErrXDR},
		{"symbol missing padding", "00000010" + "00000001" + "61", ErrXDR},
		{"vec count beyond buffer", "00000011" + "00000001" + "7fffffff", ErrXDR},
		{"vec element truncated", "00000011" + "00000001" + "00000001" + "00000003", ErrXDR},
		{"map value missing", "00000012" + "00000001" + "00000001" + "00000001", ErrXDR},
		{"address kind missing", "00000013", ErrXDR},
		{"account key truncated", "00000013" + "00000000" + "00000000" + strings.Repeat("00", 31), ErrXDR},
		{"contract id truncated", "00000013" + "00000001" + strings.Repeat("00", 16), ErrXDR},
		{"unknown address kind", "00000013" + "00000007" + zero32, ErrScValue},
		{"error value", "00000002" + "00000000" + "00000005", ErrScValue},
		{"unsupported type", "00000007", ErrScValue},
		{"negative type", "ffffffff", ErrScValue},
		{"unknown type inside vec", "00000011" + "00000001" + "00000001" + "00000063", ErrScValue},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			buf, err := hex.DecodeString(tc.hex)
			if err != nil {
				t.Fatalf("bad test hex: %v", err)
			}
			if _, err := DecodeScVal(buf); !errors.Is(err, tc.sentinel) {
				t.Fatalf("expected %v, got %v", tc.sentinel, err)
			}
		})
	}
}

func TestDecodeScValAbsentVecAndMapAreEmpty(t *testing.T) {
	// The optional-array flag may be 0 on the wire; that decodes to an empty
	// collection rather than an error.
	for _, tc := range []struct {
		name string
		hex  string
		typ  int32
	}{
		{"vec", "00000011" + "00000000", scvVec},
		{"map", "00000012" + "00000000", scvMap},
	} {
		t.Run(tc.name, func(t *testing.T) {
			buf, _ := hex.DecodeString(tc.hex)
			v, err := DecodeScVal(buf)
			if err != nil {
				t.Fatalf("decode: %v", err)
			}
			if v.Type != tc.typ || len(v.Vec) != 0 || len(v.Map) != 0 {
				t.Fatalf("decoded %+v, want an empty %s", v, tc.name)
			}
		})
	}
}

func TestDecodeScValRejectsHugeLengthWithoutAllocating(t *testing.T) {
	// A corrupt count must be rejected by the length guard, not by running out
	// of memory trying to pre-size the slice.
	for _, prefix := range []string{"00000011", "00000012"} {
		buf, _ := hex.DecodeString(prefix + "00000001" + "ffffffff")
		if _, err := DecodeScVal(buf); !errors.Is(err, ErrXDR) {
			t.Fatalf("%s: expected ErrXDR, got %v", prefix, err)
		}
	}
}

// ── Strkey ────────────────────────────────────────────────────────────────────

func TestStrkeyRoundTripsArbitraryPayloads(t *testing.T) {
	payloads := map[string][]byte{
		"all zero": make([]byte, 32),
		"all ff":   bytes.Repeat([]byte{0xff}, 32),
		"ascending": func() []byte {
			b := make([]byte, 32)
			for i := range b {
				b[i] = byte(i)
			}
			return b
		}(),
	}
	for name, raw := range payloads {
		for kind, label := range map[StrkeyKind]string{StrkeyAccount: "account", StrkeyContract: "contract"} {
			kind := kind
			t.Run(name+" "+label, func(t *testing.T) {
				s, err := EncodeStrkey(raw, kind)
				if err != nil {
					t.Fatalf("EncodeStrkey: %v", err)
				}
				if len(s) != 56 {
					t.Fatalf("strkey length = %d, want 56", len(s))
				}
				wantPrefix := map[StrkeyKind]byte{StrkeyAccount: 'G', StrkeyContract: 'C'}[kind]
				if s[0] != wantPrefix {
					t.Fatalf("strkey %q should start with %c", s, wantPrefix)
				}
				back, gotKind, err := DecodeStrkey(s)
				if err != nil {
					t.Fatalf("DecodeStrkey: %v", err)
				}
				if gotKind != kind || !bytes.Equal(back, raw) {
					t.Fatalf("round-trip = (%x, %v), want (%x, %v)", back, gotKind, raw, kind)
				}

				// Address ScVals built from the strkey survive the wire.
				enc, err := EncodeScVal(Addr(s))
				if err != nil {
					t.Fatalf("EncodeScVal: %v", err)
				}
				dec, err := DecodeScVal(enc)
				if err != nil {
					t.Fatalf("DecodeScVal: %v", err)
				}
				if dec.Addr != s {
					t.Fatalf("address round-trip = %q, want %q", dec.Addr, s)
				}
			})
		}
	}
}

func TestDecodeStrkeyAcceptsLowercase(t *testing.T) {
	raw, kind, err := DecodeStrkey(strings.ToLower(zeroContract))
	if err != nil {
		t.Fatalf("DecodeStrkey: %v", err)
	}
	if kind != StrkeyContract || !bytes.Equal(raw, make([]byte, 32)) {
		t.Fatalf("decoded (%x, %v)", raw, kind)
	}
}

func TestDecodeStrkeyErrorsAreClassified(t *testing.T) {
	cases := map[string]string{
		"empty":         "",
		"not base32":    "G0000000",
		"too long":      zeroAccount + "AAAA",
		"bad checksum":  zeroAccount[:55] + "A",
		"secret seed":   "SAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
		"muxed account": "MAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
	}
	for name, s := range cases {
		t.Run(name, func(t *testing.T) {
			raw, kind, err := DecodeStrkey(s)
			if !errors.Is(err, ErrXDR) {
				t.Fatalf("expected ErrXDR, got %v", err)
			}
			if raw != nil || kind != StrkeyUnknown {
				t.Fatalf("a rejected strkey must yield (nil, StrkeyUnknown), got (%x, %v)", raw, kind)
			}
		})
	}
}

func TestCRC16XModemKnownVector(t *testing.T) {
	// CRC-16/XMODEM of "123456789" is 0x31C3; strkeys carry it little-endian.
	got := crc16XModem([]byte("123456789"))
	if got != [2]byte{0xc3, 0x31} {
		t.Fatalf("crc16XModem = %x, want c331", got)
	}
	if got := crc16XModem(nil); got != [2]byte{0, 0} {
		t.Fatalf("crc16XModem(nil) = %x, want 0000", got)
	}
}
