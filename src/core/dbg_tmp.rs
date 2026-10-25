#[cfg(test)]
#[test]
pub fn dbg_parse() {
    use crate::core::conf::{tokenize, Reader, Tok};
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/do/schemes");
    let text = std::fs::read_to_string(dir.join("plastic.sch")).unwrap();
    let toks = tokenize(&text);
    let mut r = Reader::new(&toks);
    // mimic Scheme::parse outer loop with tracing at top-level dispatch
    let mut steps = 0usize;
    while let Some(tok) = r.peek().cloned() {
        steps += 1;
        if steps > 20000 { panic!("runaway"); }
        match tok {
            Tok::Word(w) => {
                if w == "name" || w == "author" {
                    let is_str = matches!(r.toks.get(r.pos + 1), Some(Tok::Str(_)));
                    if is_str { r.pos += 1; let _ = r.str(); } else { r.pos += 1; }
                } else { r.pos += 1; }
            }
            Tok::Open(name) if name == "Materials" => {
                eprintln!("T{steps}: Materials at {}", r.pos);
                r.pos += 1;
                // read_kv_int count: scan up to 8
                for _ in 0..8 {
                    match r.peek().cloned() {
                        Some(Tok::Word(k)) if k == "count" => { r.pos += 1; let _ = r.int(); break; }
                        Some(Tok::Word(_)) => { r.pos += 2; }
                        _ => break,
                    }
                }
                // materials boxes (7 of them): emulate skip via generic loops
                loop {
                    match r.peek().cloned() {
                        Some(Tok::Open(n)) if n.starts_with("Material ") => {
                            r.pos += 1;
                            loop {
                                match r.peek().cloned() {
                                    Some(Tok::Word(_)) => { r.pos += 1; let _ = r.word(); /* value consumption varies */ 
                                        // real code per-key consumes; emulate by consuming next Str or int Word
                                        match r.peek().cloned() {
                                            Some(Tok::Str(_)) => { r.pos += 1; }
                                            Some(Tok::Word(x)) => { if x=="none" { r.pos+=1; } else { r.pos+=1; } }
                                            _ => {}
                                        }
                                    }
                                    Some(Tok::Close(_)) => { r.pos += 1; break; }
                                    None => break,
                                    _ => { r.pos += 1; }
                                }
                            }
                        }
                        _ => break,
                    }
                }
                // skip_section("")
                let mut depth = 1i32;
                while r.pos < r.toks.len() && depth > 0 {
                    match &r.toks[r.pos] {
                        Tok::Open(_) => depth += 1,
                        Tok::Close(_) => depth -= 1,
                        _ => {}
                    }
                    r.pos += 1;
                }
                eprintln!("   after Materials skip: pos={} tok={:?}", r.pos, r.peek());
            }
            Tok::Open(name) if name == "Segments" => {
                eprintln!("T{steps}: Segments at {}", r.pos);
                r.pos += 1;
                break;
            }
            Tok::Open(other) => {
                eprintln!("T{steps}: TOP SKIP '{other}' at {}", r.pos);
                r.pos += 1;
                let o = other.clone();
                let mut depth = 1usize;
                while r.pos < r.toks.len() && depth > 0 {
                    match &r.toks[r.pos] {
                        Tok::Open(x) if *x == o => depth += 1,
                        Tok::Close(x) if *x == o => depth -= 1,
                        _ => {}
                    }
                    r.pos += 1;
                }
                eprintln!("   after skip '{o}': pos={}", r.pos);
            }
            _ => { r.pos += 1; }
        }
    }
    eprintln!("stopped at pos={} tok={:?}", r.pos, r.peek());
}
