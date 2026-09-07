//! JSON の「形」だけを要約する。
//!
//! 値ではなくキー名・型・配列の要素数を返すので、1 MB のレスポンスでも出力は数行に収まる。
//! 巨大な JSON の構造を把握するのに、本文をまるごと読ませる必要をなくすのが狙い。

use serde_json::Value;

/// 既定の再帰上限。これを超えた階層は `object` / `array` とだけ表示する。
pub const DEFAULT_MAX_DEPTH: usize = 6;

/// この長さに収まるなら 1 行で描く。
const INLINE_WIDTH: usize = 72;

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Null,
    Bool,
    Number,
    String,
    /// 型が混在する配列やフィールド。`string|null` のように描く。
    Union(Vec<Shape>),
    Array {
        len: usize,
        elem: Box<Shape>,
    },
    Object(Vec<(String, Shape)>),
    /// 再帰上限に達した位置。
    Elided(&'static str),
}

pub fn of(value: &Value) -> Shape {
    shape_at(value, DEFAULT_MAX_DEPTH)
}

pub fn of_with_depth(value: &Value, max_depth: usize) -> Shape {
    shape_at(value, max_depth)
}

fn shape_at(value: &Value, depth: usize) -> Shape {
    match value {
        Value::Null => Shape::Null,
        Value::Bool(_) => Shape::Bool,
        Value::Number(_) => Shape::Number,
        Value::String(_) => Shape::String,
        Value::Array(items) => {
            if depth == 0 {
                // **要素数は捨てない。** 深さの上限は「中を展開しない」という指定であって、
                // 「この配列について何も言わない」ではない。ここで長さまで落とすと
                // 0 件の配列と 30 件の配列が同じ `array` になり、
                // 「いま何件あるか」という一番よく要る情報が深さを上げないと取れなくなる。
                let elem = if items.is_empty() {
                    Shape::Elided("empty")
                } else {
                    Shape::Elided("…")
                };
                return Shape::Array {
                    len: items.len(),
                    elem: Box::new(elem),
                };
            }
            // 全要素を畳んで 1 つの要素型にする。要素ごとにキーが違う配列でも
            // 「どのキーが現れうるか」が 1 行で分かる。
            let elem = items
                .iter()
                .map(|v| shape_at(v, depth - 1))
                .reduce(merge)
                .unwrap_or(Shape::Elided("empty"));
            Shape::Array {
                len: items.len(),
                elem: Box::new(elem),
            }
        }
        Value::Object(map) => {
            if depth == 0 {
                return Shape::Elided("object");
            }
            Shape::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), shape_at(v, depth - 1)))
                    .collect(),
            )
        }
    }
}

/// 2 つの形を 1 つに畳む。配列要素の型をまとめるのに使う。
fn merge(a: Shape, b: Shape) -> Shape {
    if a == b {
        return a;
    }
    match (a, b) {
        // 空の配列は要素の型を何も語らない。片方が空なら、もう片方をそのまま採る。
        // union に混ぜると `…|empty` のような、読んでも意味の取れない型が出る。
        (Shape::Elided("empty"), other) | (other, Shape::Elided("empty")) => other,
        (Shape::Object(mut xs), Shape::Object(ys)) => {
            // キーの出現順を保ちつつ、片方にしかないキーも残す。
            for (k, v) in ys {
                match xs.iter_mut().find(|(xk, _)| *xk == k) {
                    Some((_, xv)) => {
                        let merged = merge(xv.clone(), v);
                        *xv = merged;
                    }
                    None => xs.push((k, v)),
                }
            }
            Shape::Object(xs)
        }
        (Shape::Array { len: la, elem: ea }, Shape::Array { len: lb, elem: eb }) => Shape::Array {
            // 要素数が違えば代表値を出せないので、大きいほうを見せる。
            len: la.max(lb),
            elem: Box::new(merge(*ea, *eb)),
        },
        (a, b) => {
            let mut variants = Vec::new();
            flatten_union(a, &mut variants);
            flatten_union(b, &mut variants);
            if variants.len() == 1 {
                variants.pop().expect("checked len == 1")
            } else {
                Shape::Union(variants)
            }
        }
    }
}

fn flatten_union(s: Shape, out: &mut Vec<Shape>) {
    match s {
        Shape::Union(vs) => {
            for v in vs {
                flatten_union(v, out);
            }
        }
        other => {
            if !out.contains(&other) {
                out.push(other);
            }
        }
    }
}

impl Shape {
    pub fn render(&self) -> String {
        let inline = self.render_inline();
        if inline.chars().count() <= INLINE_WIDTH {
            inline
        } else {
            self.render_block(0)
        }
    }

    fn render_inline(&self) -> String {
        match self {
            Shape::Null => "null".into(),
            Shape::Bool => "bool".into(),
            Shape::Number => "number".into(),
            Shape::String => "string".into(),
            Shape::Elided(what) => (*what).into(),
            Shape::Union(vs) => vs
                .iter()
                .map(Shape::render_inline)
                .collect::<Vec<_>>()
                .join("|"),
            Shape::Array { len, elem } => {
                let unit = if *len == 1 { "item" } else { "items" };
                match **elem {
                    Shape::Elided("empty") => format!("[{len} {unit}]"),
                    _ => format!("[{len} {unit}] {}", elem.render_inline()),
                }
            }
            Shape::Object(fields) => {
                if fields.is_empty() {
                    return "{}".into();
                }
                let inner = fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", v.render_inline()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{ {inner} }}")
            }
        }
    }

    fn render_block(&self, indent: usize) -> String {
        let pad = "  ".repeat(indent);
        let inner_pad = "  ".repeat(indent + 1);
        match self {
            Shape::Object(fields) if !fields.is_empty() => {
                let lines = fields
                    .iter()
                    .map(|(k, v)| {
                        let rendered = v.render_inline();
                        let body = if rendered.chars().count() + k.chars().count() + 2 + indent * 2
                            <= INLINE_WIDTH
                        {
                            rendered
                        } else {
                            v.render_block(indent + 1)
                        };
                        format!("{inner_pad}{k}: {body}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{{\n{lines}\n{pad}}}")
            }
            Shape::Array { len, elem } => {
                let unit = if *len == 1 { "item" } else { "items" };
                match **elem {
                    Shape::Elided("empty") => format!("[{len} {unit}]"),
                    _ => format!("[{len} {unit}] {}", elem.render_block(indent)),
                }
            }
            other => other.render_inline(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(v: serde_json::Value) -> String {
        of(&v).render()
    }

    #[test]
    fn renders_scalars() {
        assert_eq!(render(json!("x")), "string");
        assert_eq!(render(json!(1)), "number");
        assert_eq!(render(json!(true)), "bool");
        assert_eq!(render(json!(null)), "null");
    }

    #[test]
    fn small_object_stays_on_one_line() {
        assert_eq!(
            render(json!({"id": "a", "n": 1})),
            "{ id: string, n: number }"
        );
    }

    #[test]
    fn array_reports_its_length_not_its_contents() {
        let items: Vec<_> = (0..1240).map(|i| json!({"id": i.to_string()})).collect();
        assert_eq!(render(json!(items)), "[1240 items] { id: string }");
    }

    #[test]
    fn array_element_keys_are_unioned_across_elements() {
        // 片方の要素にしか無いキーも落とさない。落とすと「そのキーは無い」と誤読される。
        let out = render(json!([{"a": 1}, {"b": "x"}]));
        assert!(out.contains('a'), "{out}");
        assert!(out.contains('b'), "{out}");
    }

    #[test]
    fn mixed_scalar_types_render_as_a_union() {
        assert_eq!(render(json!(["x", null])), "[2 items] string|null");
    }

    #[test]
    fn empty_containers_are_distinguishable() {
        assert_eq!(render(json!([])), "[0 items]");
        assert_eq!(render(json!({})), "{}");
    }

    #[test]
    fn singular_unit_for_one_element() {
        assert_eq!(render(json!([1])), "[1 item] number");
    }

    #[test]
    fn deep_nesting_is_elided_rather_than_recursing_forever() {
        let mut v = json!(1);
        for _ in 0..50 {
            v = json!({ "next": v });
        }
        let out = of_with_depth(&v, 3).render();
        assert!(out.contains("object"), "{out}");
        // 打ち切っているので出力は短いままであること。
        assert!(out.chars().count() < 120, "打ち切れていない: {out}");
    }

    #[test]
    fn an_empty_array_does_not_pollute_the_element_type_of_its_siblings() {
        // 実 API で出た。`labels` が 0 件の要素と 4 件の要素に分かれていて、
        // 畳んだ結果が `…|empty` になった。読んでも何の型か分からない。
        let v = json!([{"labels": []}, {"labels": [{"name": "x"}]}]);
        let out = of_with_depth(&v, 2).render();
        assert!(!out.contains("empty"), "{out}");
        assert!(out.contains("labels: [1 item] …"), "{out}");
    }

    #[test]
    fn a_cut_off_array_still_reports_how_many_elements_it_has() {
        // 打ち切りは「中を見せない」であって「件数も言わない」ではない。
        // 0 件と 30 件が同じ表示になると、深さを上げるまで件数が分からない。
        let v = json!({"empty": [], "some": [{"a": 1}, {"a": 2}]});
        let out = of_with_depth(&v, 1).render();
        assert!(out.contains("empty: [0 items]"), "{out}");
        assert!(out.contains("some: [2 items]"), "{out}");
        // 中身は出さない。
        assert!(!out.contains('a'), "中身が出ている: {out}");
    }

    #[test]
    fn wide_object_breaks_into_multiple_lines() {
        let v = json!({
            "aLongFieldNameOne": "x",
            "aLongFieldNameTwo": "x",
            "aLongFieldNameThree": "x",
            "aLongFieldNameFour": "x",
        });
        let out = render(v);
        assert!(out.contains('\n'), "折り返していない: {out}");
    }

    #[test]
    fn no_values_leak_into_the_summary() {
        // 形だけを出す。値が出ると要約の意味がないうえ、秘匿値が漏れる経路になる。
        let out = render(json!({"token": "s3cr3t-token-value", "n": 12345}));
        assert!(!out.contains("s3cr3t"), "{out}");
        assert!(!out.contains("12345"), "{out}");
    }
}
