fn main() {
    // Тёмный стиль стандартных виджетов (поля ввода, кнопки) под тёмную тему клиента
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("ошибка компиляции ui/app.slint");
}
