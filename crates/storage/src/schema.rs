// @generated automatically by Diesel CLI.

diesel::table! {
    workspaces (id) {
        id -> Text,
        name -> Text,
        path -> Text,
        is_current -> Bool,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    servers (id) {
        id -> Text,
        name -> Text,
        workspace_id -> Text,
        rel_dir -> Text,
        version -> Text,
        variant -> Text,
        jar_path -> Nullable<Text>,
        jar_size -> Nullable<BigInt>,
        checksum_algo -> Nullable<Text>,
        checksum -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
        java_major -> Nullable<BigInt>,
        required_java_major -> Nullable<BigInt>,
        ram_mib -> Nullable<Integer>,
        autostart -> Bool,
    }
}

diesel::table! {
    java_runtimes (id) {
        id -> Text,
        path -> Text,
        major -> BigInt,
        version -> Text,
        vendor -> Text,
        managed -> Bool,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    java_scan_state (key) {
        key -> Text,
        scanned_at -> BigInt,
    }
}
