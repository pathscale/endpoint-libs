# Blockchain scalar decoding

Blockchain addresses and transaction hashes deserialize from canonical hexadecimal
strings. Addresses require `0x` followed by exactly 40 hexadecimal digits.
Transaction hashes require `0x` followed by exactly 64 hexadecimal digits.
Hexadecimal digits may use either case; serialization keeps the existing canonical
output.

The `WithBlockchainAddress` and `WithBlockchainTransactionHash` serde modules decode
their scalar values, while `BlockchainAddress` and `BlockchainTransactionHash`
implement `Deserialize` for their wrapper forms. All four entry points reject
non-string values, missing prefixes, incorrect lengths, and non-hexadecimal input.
Malformed values return serde errors instead of reaching fixed-byte slice
constructors.
