cmake_minimum_required(VERSION 3.22.1)
project("land-bridge")

add_library(land-bridge SHARED bridge.cpp)

target_link_libraries(land-bridge
    log
    android
)
