use secwast::{core::Module, parser::ParseBuffer};

fn main() {
    let pgm = r#"module

      (memory $memory 1)
      (export "memory" (memory $memory))

      (func (export "load_first_item_in_mem") (param $num i32) (result i32)
        i32.const 0

        ;; load first item in memory and return the result
        @label "H"
        i32.load
      )

    "#;
    let pb = ParseBuffer::new(pgm).unwrap();
    let mdl: Module = secwast::parser::parse(&pb).expect("failed to parse module");
    println!("{mdl:?}")
}
