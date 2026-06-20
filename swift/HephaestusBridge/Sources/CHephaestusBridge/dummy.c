// Empty translation unit so SwiftPM/Xcode 27 always emits CHephaestusBridge.o
// for this header-only C target. Without it, Xcode 27 beta's SPM skips the
// compile phase and the downstream libtool step fails looking for the .o.
int _hephaestus_chephaestusbridge_marker = 0;