# nickel customs

`nickel-customs` is a small program for automatically checking package submissions to the
[nickel-mine](https://github.com/nickel-lang/nickel-mine).

# Releasing

This is not really suitable for public use; there's nothing that needs to be released
on `crates.io`. You can build a github release just by pushing a tag that looks like
`v1.2.3`. [nickel-mine](https://github.com/nickel-lang/nickel-mine) will use
the latest `nickel-customs` github release for its CI.
