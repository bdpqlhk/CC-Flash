# CC-Flash Android release 混淆规则。
# release 已开启 R8: 没有生效规则时, Tauri 由 Rust 侧按类名反射调用的
# MainActivity/WryActivity 会被重命名或裁掉 → 启动即崩; 且崩溃栈没有行号。
# 保留行号信息, 便于线上问题定位(同时隐藏原始源文件名)。
-keepattributes SourceFile,LineNumberTable
-renamesourcefileattribute SourceFile

# 应用入口/WebView 宿主类: Rust 通过 JNI 按名称查找, 不得混淆或裁剪
-keep class com.ccflash.app.** { *; }
-keeppackagenames com.ccflash.app.**

# WebView 注入的 JS 接口(@JavascriptInterface)按方法名反射调用
-keepclassmembers class * {
    @android.webkit.JavascriptInterface <methods>;
}
