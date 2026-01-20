# Simplify Codebase

## Summary

Apply targeted code simplifications across the codebase to improve clarity, reduce redundancy, and enhance maintainability without changing any behavior.

## Motivation

A code review identified several areas where the codebase has unnecessary complexity:
- Redundant closures that add indirection without benefit
- Duplicated error handling patterns that could be consolidated
- Unnecessary scope blocks around simple statements
- Repetitive conditional patterns that could use iterators

These are small improvements that collectively make the code more idiomatic and easier to maintain.

## Scope

**In Scope:**
- Remove unnecessary closures and inline simple expressions
- Extract duplicated `spawn_blocking` error handling into a helper
- Remove unnecessary braces around independent await statements
- Replace repetitive if-let patterns with iterator loops
- Clean up `#[allow(unused)]` attributes in test modules

**Out of Scope:**
- Architecture changes
- Feature additions
- Performance optimizations
- API changes

## Impact

- **Risk**: Low - all changes are refactoring with no behavior change
- **Testing**: Existing tests verify behavior is preserved
- **Breaking Changes**: None

## References

- Code review findings from code-simplifier agent analysis
