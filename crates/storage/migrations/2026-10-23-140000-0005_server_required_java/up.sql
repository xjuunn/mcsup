-- 服务端版本要求的 Java 大版本（版本详情 javaVersion.majorVersion），
-- 与用户通过 `server set java-version` 绑定的 java_major 分属两列：
-- 本列是版本自身的要求，java_major 是用户显式绑定。
ALTER TABLE servers ADD COLUMN required_java_major BIGINT NULL;
