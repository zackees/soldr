cmake_minimum_required(VERSION 3.20)
project(soldr_cc_cmake_acceptance LANGUAGES C CXX)

add_library(greeting STATIC greeting.c)
add_executable(soldr-cc-cmake main.cpp)
target_link_libraries(soldr-cc-cmake PRIVATE greeting)
