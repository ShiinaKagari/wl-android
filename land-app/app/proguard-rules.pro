# wl-android ProGuard rules
# Keep JNI native methods
-keepclasseswithmembernames class * {
    native <methods>;
}

# Keep Rust native library
-keep class com.land.MainActivity { *; }
-keep class com.land.TouchForwarder { *; }
