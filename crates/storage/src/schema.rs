// @generated automatically by Diesel CLI.

diesel::table! {
    servers (name) {
        name -> Text,
        dir -> Text,
        version -> Text,
        variant -> Text,
        jar_path -> Nullable<Text>,
        jar_size -> Nullable<BigInt>,
        checksum_algo -> Nullable<Text>,
        checksum -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}
