## Much faster text embedding preview

This repo contains a rust crate containing the static embedding model deployment pipeline described [here](https://www.flowercomputer.com/news/fast-static-embedding/). 

The crate exposes two functions, `encode` and `encode_single`, take a look in `lib.rs` for more info.

### `encode`
This accepts an array of strings, encoding them in parallel

### `encode_single`
This accepts a single string

## To use

Clone the repo, and reference locally as a crate in your rust project. If you want to use a preliminary python wheel version [see the documentation at `./api-py/readme.md`](./api-py/readme.md). You might need to `cargo install maturin`.

### Crate features

#### Quantization and Truncation
By default this crate provides an unquantized model truncated to 512 dimensions. To change this, change the feature setting in your cargo.toml for your `ese` dependency ([see `Cargo.toml` for all crate features](./Cargo.toml)).

### Benchmarks

For representative numbers, enable native CPU codegen for the bench run:

```sh
RUSTFLAGS="-Ctarget-cpu=native" cargo bench -p ese
```

(This used to live in `ese/.cargo/config.toml`, but that only applied when
cargo ran from inside `ese/` and made locally-built binaries non-portable,
so it is opt-in now.)

## Model weights and attribution

At build time, `build.rs` downloads the embedding model weights and tokenizer from [`sentence-transformers/static-retrieval-mrl-en-v1`](https://huggingface.co/sentence-transformers/static-retrieval-mrl-en-v1) (licensed **Apache-2.0**) and bakes a quantized/truncated copy of the weights into the compiled crate. Distributing a build artifact therefore redistributes derived weights under the upstream Apache-2.0 license. See the repository-root `NOTICE` for details.
