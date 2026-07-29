// Rendering deferred to Kotlin-side via Canvas/setSurface callback.
// ANativeWindow_fromSurface is unreliable from Rust JNI on aarch64.
// M6b: Vulkan swapchain via ash will handle rendering without JNI env hacks.
