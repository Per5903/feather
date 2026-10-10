fn main() {
    // Тёмный стиль стандартных виджетов (поля ввода, кнопки) под тёмную тему клиента
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("ошибка компиляции ui/app.slint");

    // Windows: иконка и описание в свойствах feather.exe
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icons/feather.ico")
            .set("ProductName", "Feather")
            .set("FileDescription", "Feather — лёгкий мессенджер с голосом")
            .set("CompanyName", "Feather");
        res.compile().expect("ресурсы Windows (иконка)");
    }
}
